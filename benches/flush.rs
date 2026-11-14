//! Cost of `MetricsClient::flush()` and what it does to recording threads.
//!
//! `flush()` walks all 64 stripes and holds each stripe's lock while it drains
//! that stripe. It removes count and histogram entries (gauges persist), so the
//! next record of each of those series has to register it again. Three parts:
//!
//! 1. Criterion group `flush`: one `flush()` call over 1,000, 10,000 and
//!    100,000 series of one kind (count, gauge or histogram). Only the call is
//!    timed: repopulating the map before it and dropping the returned `Vec`
//!    after it are not. Histograms hold 20 lognormal observations per series.
//!    Elements/s is series flushed per second.
//! 2. Criterion group `record_after_flush`: one pass that records each of
//!    10,000 count series once. `steady` runs the pass over series that
//!    already exist; `after_flush` runs it right after an untimed `flush()`.
//!    The difference is the cost of re-registering every series.
//! 3. Stall run (finite, not Criterion; runs only under `cargo bench`, which
//!    passes `--bench`, and ignores Criterion name filters): one thread
//!    loops over 64 counter series, one per stripe, and times every call while
//!    the main thread flushes a map holding 10,000 or 100,000 populated
//!    histogram series. It prints one JSON line per size with the flush time
//!    and the number of calls, p50, p99.9 and max call latency in microseconds
//!    outside the flush (`quiet_*`) and during it (`in_flush_*`). A call that
//!    needs the stripe the flush is draining waits for that whole stripe, so
//!    the stall shows in the in-flush p99.9 and max. The in-flush p50 does not
//!    show it: most calls in that window land on stripes that are not locked.
//!
//! Criterion sizes its samples from wall-clock time including the untimed
//! setup, so the large cases run one iteration per sample. Repopulating
//! 100,000 histogram series takes about a second, so the 100,000-series cases
//! are capped at 10 samples and Criterion may still warn that it cannot
//! complete them in a short `--measurement-time`; the reported flush time is
//! unaffected.
use std::borrow::Cow;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chalk_metrics::client::{self, MetricsClient};
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group};

const HISTOGRAM_VALUES_PER_SERIES: usize = 20;
const LATENCY_TABLE_LEN: usize = 4_096;

fn local_client() -> MetricsClient {
    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .build_local()
}

/// A client that keeps idle series for `flushes` flushes.
fn retaining_client(flushes: u32) -> MetricsClient {
    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .retain_idle_series(flushes)
        .build_local()
}

/// Well-distributed tag hashes, so the series spread evenly over the stripes.
fn series_hashes(series: usize) -> Vec<u64> {
    use std::hash::{Hash, Hasher};
    (0..series as u64)
        .map(|i| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            i.hash(&mut h);
            h.finish()
        })
        .collect()
}

fn series_tags(i: usize) -> Vec<(&'static str, Cow<'static, str>)> {
    vec![
        ("endpoint", Cow::Owned(format!("/api/v1/resource/{i}"))),
        ("status", Cow::Borrowed("success")),
    ]
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

/// Request-latency-like values in seconds: lognormal with a 20 ms median and
/// sigma 0.8, so a long right tail. The same table on every run.
fn latency_table() -> Vec<f64> {
    let mut rng = Rng(0x9E3779B97F4A7C15);
    (0..LATENCY_TABLE_LEN)
        .map(|_| (0.020f64.ln() + 0.8 * rng.normal()).exp())
        .collect()
}

/// Records every series once per round. Each histogram round uses different
/// values; `latencies` is read only for histograms.
fn record_pass(
    client: &MetricsClient,
    kind: &str,
    hashes: &[u64],
    latencies: &[f64],
    rounds: usize,
) {
    for round in 0..rounds {
        for (i, &hash) in hashes.iter().enumerate() {
            let tags = || series_tags(i);
            match kind {
                "count" => client.record_count("flush_bench", &["http"], hash, tags, 1),
                "gauge" => client.record_gauge("flush_bench", &["http"], hash, tags, 1.0),
                _ => {
                    let value = latencies[(i + round * 131) & (LATENCY_TABLE_LEN - 1)];
                    client.record_histogram("flush_bench", &["http"], hash, tags, value);
                }
            }
        }
    }
}

/// Gives every series something to flush: one record for counts and gauges,
/// 20 observations for histograms.
fn populate(client: &MetricsClient, kind: &str, hashes: &[u64], latencies: &[f64]) {
    let rounds = if kind == "histogram" {
        HISTOGRAM_VALUES_PER_SERIES
    } else {
        1
    };
    record_pass(client, kind, hashes, latencies, rounds);
}

fn flush_benchmarks(c: &mut Criterion) {
    let latencies = latency_table();
    let mut group = c.benchmark_group("flush");

    // Largest size last: its sample-count cap then applies to nothing else.
    for series in [1_000usize, 10_000, 100_000] {
        if series == 100_000 {
            // Repopulating 100,000 histogram series takes about a second per
            // iteration, so cap the samples. This overrides `--sample-size`.
            group.sample_size(10);
        }
        for kind in ["count", "gauge", "histogram"] {
            let hashes = series_hashes(series);
            group.throughput(Throughput::Elements(series as u64));
            group.bench_function(BenchmarkId::new(kind, format!("{series}_series")), |b| {
                let client = local_client();
                b.iter_custom(|iters| {
                    let mut elapsed = Duration::ZERO;
                    for _ in 0..iters {
                        populate(&client, kind, &hashes, &latencies);
                        let start = Instant::now();
                        let flushed = black_box(client.flush());
                        elapsed += start.elapsed();
                        assert_eq!(flushed.len(), series);
                        drop(flushed);
                    }
                    elapsed
                });
                client.shutdown();
            });
        }
    }

    // One flush over 100,000 retained count series that nobody recorded since
    // the previous flush: the walk emits nothing and evicts nothing.
    let hashes = series_hashes(100_000);
    group.throughput(Throughput::Elements(100_000));
    group.bench_function(BenchmarkId::new("idle_retained", "100000_series"), |b| {
        let client = retaining_client(u32::MAX);
        populate(&client, "count", &hashes, &latencies);
        drop(client.flush());
        b.iter(|| assert!(black_box(client.flush()).is_empty()));
        client.shutdown();
    });

    group.finish();
}

fn record_after_flush_benchmarks(c: &mut Criterion) {
    const SERIES: usize = 10_000;
    let hashes = series_hashes(SERIES);
    let mut group = c.benchmark_group("record_after_flush");
    group.throughput(Throughput::Elements(SERIES as u64));

    group.bench_function("steady", |b| {
        let client = local_client();
        populate(&client, "count", &hashes, &[]);
        b.iter(|| record_pass(&client, "count", &hashes, &[], 1));
        client.shutdown();
    });

    group.bench_function("after_flush", |b| {
        let client = local_client();
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                populate(&client, "count", &hashes, &[]);
                drop(black_box(client.flush()));
                let start = Instant::now();
                record_pass(&client, "count", &hashes, &[], 1);
                elapsed += start.elapsed();
            }
            elapsed
        });
        client.shutdown();
    });

    group.bench_function("after_flush_retained", |b| {
        let client = retaining_client(4);
        b.iter_custom(|iters| {
            let mut elapsed = Duration::ZERO;
            for _ in 0..iters {
                populate(&client, "count", &hashes, &[]);
                drop(black_box(client.flush()));
                let start = Instant::now();
                record_pass(&client, "count", &hashes, &[], 1);
                elapsed += start.elapsed();
            }
            elapsed
        });
        client.shutdown();
    });

    group.finish();
}

/// A latency buffer that never page-faults or reallocates while the recorder
/// is being timed: every page is written before use, and the recorder stops
/// pushing at capacity. (`vec![0; n]` would not do: zeroed pages are mapped
/// lazily and fault on first write.)
fn pretouched(capacity: usize) -> Vec<u32> {
    let mut buffer = vec![u32::MAX; capacity];
    black_box(buffer.as_mut_slice());
    buffer.clear();
    buffer
}

/// `"<prefix>_calls":..,"<prefix>_p50_us":..,"<prefix>_p99_9_us":..,"<prefix>_max_us":..`
fn window_json(prefix: &str, mut nanos: Vec<u32>) -> String {
    assert!(!nanos.is_empty(), "no calls in the {prefix} window");
    nanos.sort_unstable();
    let quantile_us = |p: f64| nanos[((nanos.len() - 1) as f64 * p) as usize] as f64 / 1e3;
    format!(
        "\"{prefix}_calls\":{},\"{prefix}_p50_us\":{:.3},\"{prefix}_p99_9_us\":{:.3},\"{prefix}_max_us\":{:.3}",
        nanos.len(),
        quantile_us(0.5),
        quantile_us(0.999),
        quantile_us(1.0),
    )
}

/// Times every call of one recording thread before, during and after a flush
/// of `series` populated histogram series, and returns one JSON line.
fn stall_run(series: usize, latencies: &[f64]) -> String {
    const QUIET_WINDOW: Duration = Duration::from_millis(100);

    let hashes = series_hashes(series);
    let client = local_client();
    populate(&client, "histogram", &hashes, latencies);
    // The recorder's series: `tags_hash` 0..64 selects each of the 64 stripes
    // once (the stripe is the low 6 bits of `tags_hash * odd constant + name
    // hash`). Registered up front so the first quiet window times updates only.
    for stripe in 0..64 {
        client.record_count("stall", &[], stripe, Vec::new, 1);
    }

    let mut quiet = pretouched(16_000_000);
    let mut in_flush = pretouched(4_000_000);
    let stop = AtomicBool::new(false);
    let flushing = AtomicBool::new(false);

    let (quiet, in_flush, flush_ms) = std::thread::scope(|s| {
        let (client, stop, flushing) = (&client, &stop, &flushing);
        let recorder = s.spawn(move || {
            let mut i = 0u64;
            while !stop.load(Ordering::Relaxed) {
                // A call is "in flush" if a flush was active when it started
                // or when it finished. Either check alone misses stalled
                // calls: one that started just before the flush and then
                // waited on a stripe the flush took is seen only by the end
                // check, and one that was still waiting or descheduled when
                // the flag was cleared is seen only by the start check.
                let before = flushing.load(Ordering::Relaxed);
                let start = Instant::now();
                client.record_count("stall", &[], i & 63, Vec::new, 1);
                let nanos = u32::try_from(start.elapsed().as_nanos()).unwrap_or(u32::MAX);
                let window = if before || flushing.load(Ordering::Relaxed) {
                    &mut in_flush
                } else {
                    &mut quiet
                };
                if window.len() < window.capacity() {
                    window.push(nanos);
                }
                i += 1;
            }
            (quiet, in_flush)
        });

        std::thread::sleep(QUIET_WINDOW);
        flushing.store(true, Ordering::Relaxed);
        let start = Instant::now();
        let flushed = client.flush();
        let flush_ms = start.elapsed().as_secs_f64() * 1e3;
        // The flush also removed the recorder's 64 counters. Keep the flag up
        // a little longer so that re-registering them counts as flush cost
        // instead of leaking into the quiet window.
        std::thread::sleep(Duration::from_millis(2));
        flushing.store(false, Ordering::Relaxed);
        std::thread::sleep(QUIET_WINDOW);
        stop.store(true, Ordering::Relaxed);
        let (quiet, in_flush) = recorder.join().unwrap();
        // Freed only now, so releasing the flushed sketches disturbs neither
        // window.
        drop(flushed);
        (quiet, in_flush, flush_ms)
    });
    client.shutdown();

    format!(
        "{{\"bench\":\"flush_stall\",\"series\":{series},\"flush_ms\":{flush_ms:.3},{},{}}}",
        window_json("quiet", quiet),
        window_json("in_flush", in_flush),
    )
}

criterion_group!(benches, flush_benchmarks, record_after_flush_benchmarks);

// `criterion_main!` would own `main`, so this is its expansion followed by the
// stall run.
fn main() {
    benches();
    Criterion::default().configure_from_args().final_summary();

    // `cargo bench` passes `--bench`; `cargo test --benches` does not, and
    // listing benches must not run a measurement either.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|arg| arg == "--bench") || args.iter().any(|arg| arg == "--list") {
        return;
    }
    let latencies = latency_table();
    for series in [10_000, 100_000] {
        println!("{}", stall_run(series, &latencies));
    }
}
