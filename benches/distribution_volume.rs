//! Finite, paced volume/latency experiments using the production UDP exporter.
//! Uses only APIs available on the original upstream revision for comparisons.
#![allow(deprecated)]

use std::net::UdpSocket;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

use chalk_metrics::client;
use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter};
use chalk_metrics::export::{ExportError, Exporter, FlushedMetric, RawDistributionPoint, TagsData};

#[derive(Default)]
struct Traffic {
    bytes: usize,
    packets: usize,
    observations: usize,
    ones: usize,
    ninety_nines: usize,
    max_packet: usize,
}

struct Receiver {
    address: String,
    stop: Arc<AtomicBool>,
    received: Arc<AtomicUsize>,
    thread: std::thread::JoinHandle<Traffic>,
}

impl Receiver {
    fn start() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let address = socket.local_addr().unwrap().to_string();
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let received = Arc::new(AtomicUsize::new(0));
        let worker_stop = Arc::clone(&stop);
        let worker_received = Arc::clone(&received);
        let thread = std::thread::spawn(move || {
            let mut traffic = Traffic::default();
            let mut buffer = [0; 65536];
            while !worker_stop.load(Ordering::Relaxed) {
                match socket.recv(&mut buffer) {
                    Ok(n) => {
                        traffic.bytes += n;
                        traffic.packets += 1;
                        traffic.max_packet = traffic.max_packet.max(n);
                        for line in std::str::from_utf8(&buffer[..n]).unwrap().lines() {
                            let (head, kind) = line.split_once('|').unwrap();
                            assert_eq!(kind.split('|').next(), Some("d"));
                            for value in head.split(':').skip(1) {
                                let value: f64 = value.parse().unwrap();
                                traffic.ones += usize::from(value == 1.0);
                                traffic.ninety_nines += usize::from(value == 99.0);
                                traffic.observations += 1;
                            }
                        }
                        worker_received.store(traffic.observations, Ordering::Relaxed);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => panic!("benchmark UDP receive failed: {error}"),
                }
            }
            traffic
        });
        Self {
            address,
            stop,
            received,
            thread,
        }
    }

    fn finish(self, expected: usize) -> Traffic {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.received.load(Ordering::Relaxed) < expected && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().unwrap()
    }
}

fn cpu_seconds() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // Linux benchmark host; includes producers, worker, and receiver threads.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_PROCESS_CPUTIME_ID, &mut time) },
        0
    );
    time.tv_sec as f64 + time.tv_nsec as f64 / 1e9
}

fn exporter(address: &str) -> StatsdExporter {
    StatsdExporter::udp(address)
        .namespace("myapp")
        .histogram_mode(HistogramExportMode::Distribution)
        .build()
        .unwrap()
}

fn tags(series: usize, tag_bytes: usize) -> Vec<Arc<TagsData>> {
    (0..series)
        .map(|i| {
            Arc::new(TagsData {
                pairs: vec![
                    ("series", i.to_string().into()),
                    ("endpoint", "x".repeat(tag_bytes).into()),
                ],
            })
        })
        .collect()
}

fn wire_volume(series: usize, tag_bytes: usize) -> String {
    let receiver = Receiver::start();
    let exporter = exporter(&receiver.address);
    let tags = tags(series, tag_bytes);
    let points: Vec<_> = (0..10_000)
        .map(|i| RawDistributionPoint {
            namespace: &["http"],
            metric_name: "request_latency",
            tags: Arc::clone(&tags[i % series]),
            value: if i % 2 == 0 { 1.0 } else { 99.0 },
        })
        .collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let cpu = cpu_seconds();
    let start = Instant::now();
    rt.block_on(exporter.export_raw_points(&points)).unwrap();
    let send_ms = start.elapsed().as_secs_f64() * 1000.0;
    let traffic = receiver.finish(points.len());
    let cpu_ms = (cpu_seconds() - cpu) * 1000.0;
    assert_eq!(
        traffic.observations,
        points.len(),
        "receiver loss invalidates the wire-volume comparison"
    );
    assert_eq!((traffic.ones, traffic.ninety_nines), (5_000, 5_000));
    assert!(traffic.max_packet <= 1432);
    format!(
        "{{\"workload\":\"wire_volume\",\"series\":{series},\"tag_bytes\":{tag_bytes},\"observations\":{},\"ones\":{},\"ninety_nines\":{},\"payload_bytes\":{},\"datagrams\":{},\"max_datagram_bytes\":{},\"send_ms\":{send_ms:.3},\"process_cpu_ms\":{cpu_ms:.3}}}",
        traffic.observations,
        traffic.ones,
        traffic.ninety_nines,
        traffic.bytes,
        traffic.packets,
        traffic.max_packet
    )
}

struct Observer {
    recorded_at: Arc<Mutex<Vec<Instant>>>,
    latencies: Arc<Mutex<Vec<Duration>>>,
}

#[async_trait::async_trait]
impl Exporter for Observer {
    async fn export(&self, _: &[FlushedMetric]) -> Result<(), ExportError> {
        Ok(())
    }
    async fn export_raw_points(&self, points: &[RawDistributionPoint]) -> Result<(), ExportError> {
        let now = Instant::now();
        let recorded_at = self.recorded_at.lock().unwrap();
        self.latencies.lock().unwrap().extend(
            points
                .iter()
                .map(|point| now.duration_since(recorded_at[point.value as usize])),
        );
        Ok(())
    }
}

fn paced(rate: usize, series: usize, tag_bytes: usize) -> String {
    // Cross the ten-second aggregate tick, then exercise final shutdown flushing.
    const SECONDS: usize = 12;
    const TICKS_PER_SECOND: usize = 200;
    let count = rate * SECONDS;
    let batch = rate / TICKS_PER_SECOND;
    let receiver = Receiver::start();
    let tags = tags(series, tag_bytes);
    let recorded_at = Arc::new(Mutex::new(vec![Instant::now(); count]));
    let latencies = Arc::new(Mutex::new(Vec::with_capacity(count)));
    let client = client::builder()
        .with_exporter(exporter(&receiver.address))
        .with_exporter(Observer {
            recorded_at: Arc::clone(&recorded_at),
            latencies: Arc::clone(&latencies),
        })
        // Original upstream needs this setting for correct raw observations.
        .aggregate_distr_metrics(false)
        .flush_interval(Duration::from_secs(10))
        .build_local();
    let cpu = cpu_seconds();
    let start = Instant::now();
    for tick in 0..SECONDS * TICKS_PER_SECOND {
        std::thread::sleep(
            (start + Duration::from_millis((tick * 5) as u64))
                .saturating_duration_since(Instant::now()),
        );
        let offset = tick * batch;
        recorded_at.lock().unwrap()[offset..offset + batch].fill(Instant::now());
        for i in offset..offset + batch {
            client.record_histogram(
                "request_latency",
                &["http"],
                (i % series) as u64,
                || tags[i % series].pairs.clone(),
                i as f64,
            );
        }
    }
    std::thread::sleep(
        (start + Duration::from_secs(SECONDS as u64)).saturating_duration_since(Instant::now()),
    );
    client.shutdown();
    let mut latencies = latencies.lock().unwrap();
    latencies.sort_unstable();
    let accepted = latencies.len();
    assert!(accepted > 0);
    let p50_ms = latencies[accepted / 2].as_secs_f64() * 1000.0;
    let p99_ms = latencies[(accepted - 1) * 99 / 100].as_secs_f64() * 1000.0;
    let traffic = receiver.finish(accepted);
    let cpu_ms = (cpu_seconds() - cpu) * 1000.0;
    assert_eq!(
        traffic.observations, accepted,
        "transport loss must be distinguished from queue overflow"
    );
    assert!(traffic.max_packet <= 1432);
    format!(
        "{{\"workload\":\"paced\",\"rate_per_second\":{rate},\"seconds\":{SECONDS},\"series\":{series},\"tag_bytes\":{tag_bytes},\"recorded\":{count},\"accepted\":{accepted},\"queue_drops\":{},\"received\":{},\"payload_bytes\":{},\"datagrams\":{},\"p50_latency_ms\":{p50_ms:.3},\"p99_latency_ms\":{p99_ms:.3},\"process_cpu_ms\":{cpu_ms:.3}}}",
        count - accepted,
        traffic.observations,
        traffic.bytes,
        traffic.packets
    )
}

fn main() {
    let mut rows = Vec::new();
    for (series, tag_bytes) in [(1, 8), (64, 96), (10_000, 8)] {
        rows.push(wire_volume(series, tag_bytes));
    }
    for (rate, series, tag_bytes) in [(1_000, 1, 8), (10_000, 1, 96), (50_000, 64, 96)] {
        rows.push(paced(rate, series, tag_bytes));
    }
    println!("[\n{}\n]", rows.join(",\n"));
}
