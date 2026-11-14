//! Finite delivery experiments for the DogStatsD exporter: how much of what an
//! application records reaches a receiver on the same host at default settings.
//!
//! Prints one JSON array of rows to stdout. Each row has a `"workload"` field:
//!
//! - `raw_ceiling`: where the default configuration starts dropping. A default
//!   client (16,384-slot raw-sample queue drained every 100 ms) with a
//!   Distribution-mode `StatsdExporter` is fed by four producer threads paced to
//!   a target rate for two seconds, over UDP and over a Unix datagram socket.
//!   `queue_drops` are observations the bounded queue rejected; `transport_lost`
//!   are accepted observations that never reached the receiver. One
//!   `raw_ceiling_summary` row per transport gives the highest target rate that
//!   had zero queue drops.
//! - `aggregate_flush`: does an ordinary aggregate flush arrive? A batch of
//!   counters and gauges is passed to `Exporter::export` five times and the
//!   lines that reach the receiver are counted after each call.
//! - `wire_bytes`: bytes on the wire per raw observation for four value shapes,
//!   from the `1`/`99` values `distribution_volume` uses to full-precision
//!   latencies.
//!
//! Loss is the measured outcome here, so it is reported, not asserted. The only
//! assertions are sanity checks: the receiver never sees more than was sent, and
//! no datagram exceeds the exporter's limit (1,432 bytes UDP, 8,192 bytes UDS).
//!
//! Results depend on the host's kernel socket buffer settings
//! (`net.core.rmem_default`, `net.core.wmem_default`, `net.unix.max_dgram_qlen`)
//! and on how promptly the receiver thread is scheduled, so compare runs from
//! the same machine only. Export errors from the client's worker are logged to
//! stderr, as is the total runtime; stdout carries only the JSON.
//!
//! Run with `cargo bench --bench delivery`; it takes about 25 seconds.

use std::borrow::Cow;
use std::net::UdpSocket;
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chalk_metrics::client;
use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter, StatsdExporterBuilder};
use chalk_metrics::export::{
    Exporter, FlushedMetric, FlushedValue, RawDistributionPoint, TagsData,
};
use tokio::runtime::Runtime;

const PRODUCERS: usize = 4;
const SERIES: usize = 64;
const SECONDS: usize = 2;
const RATES: [usize; 5] = [50_000, 100_000, 150_000, 200_000, 400_000];
const FLUSH_SERIES: [usize; 2] = [1_000, 10_000];
const FLUSH_RUNS: usize = 5;
const WIRE_OBSERVATIONS: usize = 10_000;
/// Longest wait for datagrams still queued in the kernel after sending ends.
const SETTLE_LIMIT: Duration = Duration::from_secs(2);
/// The receiver is considered drained once nothing new arrives for this long.
const SETTLE_QUIET: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
enum Transport {
    Udp,
    #[cfg(unix)]
    Uds,
}

const TRANSPORTS: &[Transport] = &[
    Transport::Udp,
    #[cfg(unix)]
    Transport::Uds,
];

impl Transport {
    fn name(self) -> &'static str {
        match self {
            Transport::Udp => "udp",
            #[cfg(unix)]
            Transport::Uds => "uds",
        }
    }

    /// The exporter's default datagram limit for this transport.
    fn max_datagram(self) -> usize {
        match self {
            Transport::Udp => 1432,
            #[cfg(unix)]
            Transport::Uds => 8192,
        }
    }
}

/// A Unix socket path that is removed when the receiver goes away, including
/// when a sanity assertion unwinds.
#[cfg(unix)]
struct SocketFile(PathBuf);

#[cfg(unix)]
impl Drop for SocketFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

enum Endpoint {
    Udp(String),
    #[cfg(unix)]
    Uds(SocketFile),
}

#[derive(Clone, Copy, Default)]
struct Traffic {
    datagrams: usize,
    bytes: usize,
    lines: usize,
    /// Values carried by distribution (`|d`) lines.
    observations: usize,
    max_datagram: usize,
}

/// A draining receiver that decodes every datagram on its own thread.
struct Receiver {
    endpoint: Endpoint,
    stop: Arc<AtomicBool>,
    traffic: Arc<Mutex<Traffic>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Receiver {
    fn start(transport: Transport) -> Self {
        const READ_TIMEOUT: Option<Duration> = Some(Duration::from_millis(20));
        let stop = Arc::new(AtomicBool::new(false));
        let traffic = Arc::new(Mutex::new(Traffic::default()));
        let (endpoint, thread) = match transport {
            Transport::Udp => {
                let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
                socket.set_read_timeout(READ_TIMEOUT).unwrap();
                let address = socket.local_addr().unwrap().to_string();
                let thread = spawn_reader(move |buffer| socket.recv(buffer), &stop, &traffic);
                (Endpoint::Udp(address), thread)
            }
            #[cfg(unix)]
            Transport::Uds => {
                let file = SocketFile(std::env::temp_dir().join(format!(
                    "chalk-metrics-delivery-{}.sock",
                    std::process::id()
                )));
                let _ = std::fs::remove_file(&file.0);
                let socket = UnixDatagram::bind(&file.0).unwrap();
                socket.set_read_timeout(READ_TIMEOUT).unwrap();
                let thread = spawn_reader(move |buffer| socket.recv(buffer), &stop, &traffic);
                (Endpoint::Uds(file), thread)
            }
        };
        Self {
            endpoint,
            stop,
            traffic,
            thread: Some(thread),
        }
    }

    /// A Distribution-mode exporter builder pointed at this receiver.
    fn exporter(&self) -> StatsdExporterBuilder {
        let builder = match &self.endpoint {
            Endpoint::Udp(address) => StatsdExporter::udp(address.as_str()),
            #[cfg(unix)]
            Endpoint::Uds(file) => StatsdExporter::uds(&file.0),
        };
        builder
            .namespace("myapp")
            .histogram_mode(HistogramExportMode::Distribution)
    }

    fn traffic(&self) -> Traffic {
        *self.traffic.lock().unwrap()
    }

    /// Call after sending has finished. Waits until `progress` reaches `target`,
    /// stops growing, or `SETTLE_LIMIT` passes, then returns what arrived.
    fn settle(&self, target: usize, progress: impl Fn(&Traffic) -> usize) -> Traffic {
        let deadline = Instant::now() + SETTLE_LIMIT;
        let mut last = self.traffic();
        let mut last_change = Instant::now();
        loop {
            let now = Instant::now();
            if progress(&last) >= target || now >= deadline || now - last_change >= SETTLE_QUIET {
                return last;
            }
            std::thread::sleep(Duration::from_millis(5));
            let current = self.traffic();
            if progress(&current) != progress(&last) {
                last_change = Instant::now();
            }
            last = current;
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let result = self.thread.take().unwrap().join();
        if !std::thread::panicking() {
            result.expect("benchmark receiver thread panicked");
        }
    }
}

fn spawn_reader(
    mut recv: impl FnMut(&mut [u8]) -> std::io::Result<usize> + Send + 'static,
    stop: &Arc<AtomicBool>,
    traffic: &Arc<Mutex<Traffic>>,
) -> std::thread::JoinHandle<()> {
    let (stop, traffic) = (Arc::clone(stop), Arc::clone(traffic));
    std::thread::spawn(move || {
        let mut buffer = vec![0; 65536];
        while !stop.load(Ordering::Relaxed) {
            match recv(&mut buffer) {
                Ok(n) => {
                    let (lines, observations) = decode(&buffer[..n]);
                    let mut traffic = traffic.lock().unwrap();
                    traffic.datagrams += 1;
                    traffic.bytes += n;
                    traffic.lines += lines;
                    traffic.observations += observations;
                    traffic.max_datagram = traffic.max_datagram.max(n);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("benchmark receive failed: {error}"),
            }
        }
    })
}

/// Parses one datagram as DogStatsD, returning its line count and the number
/// of values carried by distribution lines.
fn decode(datagram: &[u8]) -> (usize, usize) {
    let (mut lines, mut observations) = (0, 0);
    for line in std::str::from_utf8(datagram).unwrap().lines() {
        let (head, rest) = line.split_once('|').unwrap();
        let mut values = 0;
        for value in head.split(':').skip(1) {
            value.parse::<f64>().unwrap();
            values += 1;
        }
        assert!(values > 0, "metric line without a value: {line}");
        lines += 1;
        if rest.split('|').next() == Some("d") {
            observations += values;
        }
    }
    (lines, observations)
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

fn sleep_until(deadline: Instant) {
    std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
}

/// xorshift64* generator: deterministic, no dependencies.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    /// Uniform in (0, 1).
    fn unit(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64 + 0.5) / (1u64 << 53) as f64
    }

    /// Standard normal (Box-Muller).
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.unit(), self.unit());
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

/// Request-latency-like values in seconds: lognormal, median 20 ms, sigma 0.8.
fn latency_table() -> Vec<f64> {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    (0..4096)
        .map(|_| (0.020f64.ln() + 0.8 * rng.normal()).exp())
        .collect()
}

fn json_string(text: &str) -> String {
    let mut out = String::from('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// One JSON object; `fields` values are already JSON-encoded.
fn json_row(workload: &str, fields: &[(&str, String)]) -> String {
    let mut out = format!("{{\"workload\":\"{workload}\"");
    for (key, value) in fields {
        out.push_str(&format!(",\"{key}\":{value}"));
    }
    out.push('}');
    out
}

/// `null` when nothing was received, so a total loss is not shown as zero cost.
fn bytes_per_observation(traffic: &Traffic) -> String {
    if traffic.observations == 0 {
        "null".to_owned()
    } else {
        format!("{:.2}", traffic.bytes as f64 / traffic.observations as f64)
    }
}

/// Returns the row and the number of queue drops.
fn raw_ceiling(transport: Transport, rate: usize, latencies: &[f64]) -> (String, usize) {
    let receiver = Receiver::start(transport);
    // Default client: queue capacity and flush intervals are left untouched.
    let client = client::builder()
        .with_exporter(receiver.exporter().build().unwrap())
        .build_local();
    let per_producer = rate / PRODUCERS;
    let recorded = per_producer * SECONDS * PRODUCERS;
    let cpu = cpu_seconds();
    let start = Instant::now();
    std::thread::scope(|scope| {
        for producer in 0..PRODUCERS {
            let client = &client;
            scope.spawn(move || {
                let mut i = producer * 7919;
                let mut sent = 0;
                // Absolute 1 ms deadlines: a late tick catches up, it does not
                // lower the rate.
                for tick in 0..SECONDS * 1000 {
                    sleep_until(start + Duration::from_millis(tick as u64));
                    let due = per_producer * (tick + 1) / 1000;
                    while sent < due {
                        let series = i % SERIES;
                        client.record_histogram(
                            "request_latency",
                            &["http"],
                            series as u64,
                            || {
                                vec![
                                    ("endpoint", Cow::Owned(format!("/api/v1/resource/{series}"))),
                                    ("status", Cow::Borrowed("success")),
                                ]
                            },
                            latencies[i % latencies.len()],
                        );
                        i += 1;
                        sent += 1;
                    }
                }
                sleep_until(start + Duration::from_secs(SECONDS as u64));
            });
        }
    });
    let elapsed = start.elapsed().as_secs_f64();
    client.shutdown();
    let queue_drops = client.dropped_raw_samples() as usize;
    let accepted = recorded - queue_drops;
    let traffic = receiver.settle(accepted, |traffic| traffic.observations);
    let cpu_ms = (cpu_seconds() - cpu) * 1000.0;
    assert!(
        traffic.observations <= accepted,
        "received {} observations but only {accepted} were accepted",
        traffic.observations
    );
    assert!(traffic.max_datagram <= transport.max_datagram());
    let row = json_row(
        "raw_ceiling",
        &[
            ("transport", json_string(transport.name())),
            ("target_rate", rate.to_string()),
            ("achieved_rate", format!("{:.0}", recorded as f64 / elapsed)),
            ("recorded", recorded.to_string()),
            ("queue_drops", queue_drops.to_string()),
            ("accepted", accepted.to_string()),
            ("received", traffic.observations.to_string()),
            (
                "transport_lost",
                (accepted - traffic.observations).to_string(),
            ),
            ("payload_bytes", traffic.bytes.to_string()),
            ("datagrams", traffic.datagrams.to_string()),
            ("bytes_per_observation", bytes_per_observation(&traffic)),
            ("process_cpu_ms", format!("{cpu_ms:.3}")),
        ],
    );
    (row, queue_drops)
}

/// 70% counters and 30% gauges, one unique series each.
fn aggregate_batch(series: usize) -> Vec<FlushedMetric> {
    (0..series)
        .map(|i| {
            let tags = Arc::new(TagsData {
                pairs: vec![
                    ("endpoint", Cow::Owned(format!("/api/v1/resource/{i}"))),
                    (
                        "status",
                        Cow::Borrowed(if i % 7 == 0 { "failure" } else { "success" }),
                    ),
                    ("region", Cow::Owned("us-east-1".to_owned())),
                ],
            });
            let (metric_name, value) = if i % 10 < 3 {
                ("active_connections", FlushedValue::Gauge(i as f64 * 1.5))
            } else {
                ("request_count", FlushedValue::Count(i as i64 + 1))
            };
            FlushedMetric {
                namespace: &["http"],
                metric_name,
                tags,
                value,
            }
        })
        .collect()
}

fn aggregate_flush(runtime: &Runtime, transport: Transport, series: usize) -> String {
    let receiver = Receiver::start(transport);
    let exporter = receiver
        .exporter()
        .default_tag("env", "prod")
        .default_tag("service", "engine")
        .build()
        .unwrap();
    let metrics = aggregate_batch(series);
    // Counters and gauges are one line each.
    let expected = metrics.len();
    let mut delivered = Vec::with_capacity(FLUSH_RUNS);
    let mut errors = 0;
    let mut first_error = None;
    let mut datagrams = 0;
    let mut export_ms = f64::MAX;
    for _ in 0..FLUSH_RUNS {
        let before = receiver.traffic();
        let start = Instant::now();
        let result = runtime.block_on(exporter.export(&metrics));
        export_ms = export_ms.min(start.elapsed().as_secs_f64() * 1000.0);
        if let Err(error) = result {
            errors += 1;
            first_error.get_or_insert_with(|| error.to_string());
        }
        let after = receiver.settle(before.lines + expected, |traffic| traffic.lines);
        let lines = after.lines - before.lines;
        assert!(
            lines <= expected,
            "received {lines} lines but only {expected} were exported"
        );
        assert!(after.max_datagram <= transport.max_datagram());
        delivered.push(lines);
        datagrams = after.datagrams - before.datagrams;
    }
    delivered.sort_unstable();
    json_row(
        "aggregate_flush",
        &[
            ("transport", json_string(transport.name())),
            ("series", series.to_string()),
            ("expected_lines", expected.to_string()),
            ("delivered_min", delivered[0].to_string()),
            ("delivered_median", delivered[FLUSH_RUNS / 2].to_string()),
            ("delivered_max", delivered[FLUSH_RUNS - 1].to_string()),
            ("runs_with_export_error", errors.to_string()),
            (
                "first_error",
                first_error.map_or_else(|| "null".to_owned(), |error| json_string(&error)),
            ),
            ("datagrams", datagrams.to_string()),
            ("export_ms", format!("{export_ms:.3}")),
        ],
    )
}

fn wire_bytes(runtime: &Runtime, shape: &str, values: &[f64]) -> String {
    let receiver = Receiver::start(Transport::Udp);
    let exporter = receiver.exporter().build().unwrap();
    let tags = Arc::new(TagsData {
        pairs: vec![("endpoint", Cow::Borrowed("api/v1"))],
    });
    let points: Vec<_> = values
        .iter()
        .map(|&value| RawDistributionPoint {
            namespace: &["http"],
            metric_name: "request_latency",
            tags: Arc::clone(&tags),
            value,
        })
        .collect();
    if let Err(error) = runtime.block_on(exporter.export_raw_points(&points)) {
        eprintln!("delivery: wire_bytes {shape}: {error}");
    }
    let traffic = receiver.settle(points.len(), |traffic| traffic.observations);
    assert!(
        traffic.observations <= points.len(),
        "received {} observations but only {} were exported",
        traffic.observations,
        points.len()
    );
    assert!(traffic.max_datagram <= Transport::Udp.max_datagram());
    json_row(
        "wire_bytes",
        &[
            ("shape", json_string(shape)),
            ("observations", points.len().to_string()),
            ("received", traffic.observations.to_string()),
            ("payload_bytes", traffic.bytes.to_string()),
            ("datagrams", traffic.datagrams.to_string()),
            ("bytes_per_observation", bytes_per_observation(&traffic)),
        ],
    )
}

fn main() {
    let start = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let latencies = latency_table();
    let mut rows = Vec::new();

    for &transport in TRANSPORTS {
        let mut highest_zero_drop_rate = None;
        for rate in RATES {
            let (row, queue_drops) = raw_ceiling(transport, rate, &latencies);
            rows.push(row);
            if queue_drops == 0 {
                // RATES ascends, so the last zero-drop rate is the highest.
                highest_zero_drop_rate = Some(rate);
            }
        }
        rows.push(json_row(
            "raw_ceiling_summary",
            &[
                ("transport", json_string(transport.name())),
                (
                    "highest_zero_drop_rate",
                    highest_zero_drop_rate
                        .map_or_else(|| "null".to_owned(), |rate| rate.to_string()),
                ),
            ],
        ));
    }

    for &transport in TRANSPORTS {
        for series in FLUSH_SERIES {
            rows.push(aggregate_flush(&runtime, transport, series));
        }
    }

    let seconds = |i: usize| latencies[i % latencies.len()];
    let shapes: [(&str, Vec<f64>); 4] = [
        (
            "alternating_1_99",
            (0..WIRE_OBSERVATIONS)
                .map(|i| if i % 2 == 0 { 1.0 } else { 99.0 })
                .collect(),
        ),
        (
            "latency_seconds_f64",
            (0..WIRE_OBSERVATIONS).map(seconds).collect(),
        ),
        (
            "latency_seconds_4_significant_digits",
            (0..WIRE_OBSERVATIONS)
                .map(|i| format!("{:.3e}", seconds(i)).parse().unwrap())
                .collect(),
        ),
        (
            "latency_whole_microseconds",
            (0..WIRE_OBSERVATIONS)
                .map(|i| (seconds(i) * 1e6).round())
                .collect(),
        ),
    ];
    for (shape, values) in &shapes {
        rows.push(wire_bytes(&runtime, shape, values));
    }

    println!("[\n{}\n]", rows.join(",\n"));
    eprintln!(
        "delivery: total runtime {:.1} s",
        start.elapsed().as_secs_f64()
    );
}
