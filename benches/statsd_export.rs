use std::borrow::Cow;
use std::net::UdpSocket;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter};
use chalk_metrics::export::{Exporter, FlushedMetric, FlushedValue, TagsData, UDDSketch};

fn make_count_metric(i: usize) -> FlushedMetric {
    FlushedMetric {
        namespace: &["http"],
        metric_name: "request_count",
        tags: Arc::new(TagsData {
            pairs: vec![
                ("endpoint", Cow::Owned(format!("/api/v{}", i % 5))),
                ("status", Cow::Borrowed("success")),
            ],
        }),
        value: FlushedValue::Count(i as i64),
    }
}

fn make_gauge_metric(i: usize) -> FlushedMetric {
    FlushedMetric {
        namespace: &["http"],
        metric_name: "active_connections",
        tags: Arc::new(TagsData {
            pairs: vec![("endpoint", Cow::Owned(format!("/api/v{}", i % 5)))],
        }),
        value: FlushedValue::Gauge(i as f64 * 1.5),
    }
}

fn make_histogram_metric(i: usize) -> FlushedMetric {
    let mut sketch = UDDSketch::new(200, 0.001);
    for v in 0..100 {
        sketch.add_value(v as f64 * 0.01);
    }

    FlushedMetric {
        namespace: &["http"],
        metric_name: "request_latency",
        tags: Arc::new(TagsData {
            pairs: vec![
                ("endpoint", Cow::Owned(format!("/api/v{}", i % 5))),
                ("status", Cow::Borrowed("success")),
            ],
        }),
        value: FlushedValue::Histogram(sketch),
    }
}

fn make_mixed_batch(size: usize) -> Vec<FlushedMetric> {
    let mut metrics = Vec::with_capacity(size);
    for i in 0..size {
        match i % 3 {
            0 => metrics.push(make_count_metric(i)),
            1 => metrics.push(make_gauge_metric(i)),
            _ => metrics.push(make_histogram_metric(i)),
        }
    }
    metrics
}

/// Bind a UDP socket to receive datagrams and drain them in a background thread.
fn setup_drain_socket() -> (u16, std::thread::JoinHandle<()>) {
    let recv = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = recv.local_addr().unwrap().port();
    recv.set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .unwrap();

    let handle = std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        loop {
            match recv.recv(&mut buf) {
                Ok(_) => {}
                Err(_) => break, // timeout or closed
            }
        }
    });

    (port, handle)
}

fn statsd_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("statsd_export");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    for batch_size in [10, 100, 1000] {
        group.throughput(Throughput::Elements(batch_size as u64));

        // Percentiles mode
        group.bench_with_input(
            BenchmarkId::new("percentiles", batch_size),
            &batch_size,
            |b, &size| {
                let (port, _drain) = setup_drain_socket();
                let exporter = StatsdExporter::udp(format!("127.0.0.1:{port}"))
                    .namespace("myapp")
                    .histogram_mode(HistogramExportMode::Percentiles(vec![
                        0.5, 0.9, 0.95, 0.99,
                    ]))
                    .build()
                    .unwrap();
                let metrics = make_mixed_batch(size);

                b.iter(|| {
                    rt.block_on(exporter.export(&metrics)).unwrap();
                });
            },
        );

        // Distribution mode
        group.bench_with_input(
            BenchmarkId::new("distribution", batch_size),
            &batch_size,
            |b, &size| {
                let (port, _drain) = setup_drain_socket();
                let exporter = StatsdExporter::udp(format!("127.0.0.1:{port}"))
                    .namespace("myapp")
                    .histogram_mode(HistogramExportMode::Distribution)
                    .build()
                    .unwrap();
                let metrics = make_mixed_batch(size);

                b.iter(|| {
                    rt.block_on(exporter.export(&metrics)).unwrap();
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, statsd_benchmarks);
criterion_main!(benches);
