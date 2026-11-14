//! Delivery never waits on the socket: sends that would block are dropped and counted.

#![cfg(unix)]

use std::borrow::Cow;
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chalk_metrics::client;
use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter};
use chalk_metrics::export::{
    DistributionInput, ExportError, Exporter, FlushedMetric, FlushedValue, RawDistributionPoint,
    TagsData,
};

/// A bound Unix datagram socket that is removed on drop.
struct Receiver {
    socket: UnixDatagram,
    path: PathBuf,
}

impl Receiver {
    fn bind(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("chalk-metrics-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let socket = UnixDatagram::bind(&path).unwrap();
        socket.set_nonblocking(true).unwrap();
        Self { socket, path }
    }

    /// Read everything queued: (datagrams, lines, bytes).
    fn drain(&self) -> (u64, u64, u64) {
        let mut buffer = vec![0; 65536];
        let (mut datagrams, mut lines, mut bytes) = (0, 0, 0);
        while let Ok(n) = self.socket.recv(&mut buffer) {
            datagrams += 1;
            bytes += n as u64;
            lines += buffer[..n].split(|&b| b == b'\n').count() as u64;
        }
        (datagrams, lines, bytes)
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn counter_series(count: usize) -> Vec<FlushedMetric> {
    (0..count)
        .map(|i| FlushedMetric {
            namespace: &["http"],
            metric_name: "request_count",
            tags: Arc::new(TagsData {
                pairs: vec![("endpoint", Cow::Owned(format!("/api/v1/resource/{i}")))],
            }),
            value: FlushedValue::Count(i as i64 + 1),
        })
        .collect()
}

/// Exact byte length of the counter line for series `i` (see `counter_series`).
fn line_len(i: usize) -> usize {
    format!(
        "http.request_count:{}|c|#endpoint:/api/v1/resource/{i}",
        i + 1
    )
    .len()
}

#[tokio::test]
async fn full_receiver_drops_are_counted_and_export_does_not_stall() {
    const SERIES: usize = 10_000;
    let receiver = Receiver::bind("drop-accounting");
    let exporter = StatsdExporter::uds(&receiver.path).build().unwrap();
    let counters = exporter.drop_counters();
    let metrics = counter_series(SERIES);

    let start = Instant::now();
    let result = exporter.export(&metrics).await;
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "export took {:?}",
        start.elapsed()
    );

    let (datagrams, lines, bytes) = receiver.drain();
    assert!(result.is_err(), "an unread receiver must overflow");
    assert!(counters.datagrams() > 0);
    assert_eq!(lines + counters.lines(), SERIES as u64);
    assert!(counters.datagrams() <= counters.lines());
    // Every line is in exactly one datagram, and a datagram's lines are
    // separated by one newline each, so total bytes follow from the datagram count.
    let line_bytes: usize = (0..SERIES).map(line_len).sum();
    let total_datagrams = datagrams + counters.datagrams();
    assert_eq!(
        bytes + counters.bytes(),
        line_bytes as u64 + SERIES as u64 - total_datagrams
    );
}

#[test]
fn shutdown_does_not_stall_on_a_receiver_that_never_reads() {
    let receiver = Receiver::bind("shutdown");
    let exporter = StatsdExporter::uds(&receiver.path)
        .histogram_mode(HistogramExportMode::Distribution)
        .build()
        .unwrap();
    let counters = exporter.drop_counters();
    let client = client::builder().with_exporter(exporter).build_local();
    for i in 0..50_000 {
        client.record_histogram("latency", &[], 7, Vec::new, i as f64);
    }
    let start = Instant::now();
    client.shutdown();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "shutdown took {:?}",
        start.elapsed()
    );
    // Whatever the receiver did not accept is counted, not silently lost.
    let (_, lines, _) = receiver.drain();
    assert!(lines + counters.lines() > 0);
}

/// Counts the raw points it is asked to export.
struct CountingExporter(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl Exporter for CountingExporter {
    fn distribution_input(&self) -> Option<DistributionInput> {
        Some(DistributionInput::Raw)
    }

    async fn export(&self, _metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        Ok(())
    }

    async fn export_raw_points(&self, points: &[RawDistributionPoint]) -> Result<(), ExportError> {
        self.0.fetch_add(points.len(), Ordering::SeqCst);
        Ok(())
    }
}

fn wait_for(deadline: Duration, condition: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    condition()
}

#[test]
fn raw_queue_drains_early_once_a_quarter_full() {
    // The early check runs every interval / 10 (200 ms here), far sooner than
    // the 2 s regular raw flush.
    let exported = Arc::new(AtomicUsize::new(0));
    let client = client::builder()
        .with_exporter(CountingExporter(Arc::clone(&exported)))
        .raw_sample_flush_interval(Duration::from_secs(2))
        .flush_interval(Duration::from_secs(3600))
        .build_local();

    // Under a quarter of the default 16,384-slot queue: nothing is exported early.
    for i in 0..100 {
        client.record_histogram("latency", &[], 1, Vec::new, i as f64);
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(exported.load(Ordering::SeqCst), 0);

    // Half the queue in total: an early drain happens long before the regular
    // flush. It may catch the queue while this loop is still filling it, so
    // only the quarter-full level is guaranteed to have been exported early.
    for i in 0..8_092 {
        client.record_histogram("latency", &[], 1, Vec::new, i as f64);
    }
    assert!(
        wait_for(Duration::from_millis(500), || exported
            .load(Ordering::SeqCst)
            >= 4_096),
        "exported {} points early",
        exported.load(Ordering::SeqCst)
    );
    assert_eq!(client.dropped_raw_samples(), 0);
    client.shutdown();
    assert_eq!(exported.load(Ordering::SeqCst), 8_192);
}
