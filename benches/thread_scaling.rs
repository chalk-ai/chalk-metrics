//! Recording throughput as the number of recording threads grows.
//!
//! The `contention` group in `metrics_throughput.rs` covers one 80-series pool
//! at 2 and 8 threads and has no single-thread point, so it cannot show how
//! throughput scales. This bench runs every scenario at 1, 2, 4, 8 and 16
//! threads.
//!
//! What is timed: every thread waits on a barrier, then makes `iters * 1,000`
//! recording calls and times its own loop. A sample's duration is the slowest
//! thread's time; thread spawn and join are outside it. One Criterion iteration
//! is therefore 1,000 calls on each of `n` threads.
//!
//! How to read it: elements/s is total throughput summed across threads
//! (`1,000 * n` calls per iteration). Perfect scaling multiplies it by the
//! thread count; a flat or falling line means the threads are serialized on a
//! shared stripe lock or cache line. The time per iteration is how long each
//! thread took for its 1,000 calls, so divide it by 1,000 for the per-call
//! latency one thread sees.
//!
//! Scenarios (all use the low-level client API with precomputed tag hashes, on
//! series that already exist after the first call):
//! - `count/one_series`: every thread increments the same series.
//! - `count/series_per_thread`: each thread has its own series on its own
//!   stripe, so the threads share no lock.
//! - `count/pool_80`: the 80-series pool of `metrics_throughput.rs`, each
//!   thread walking it from a different offset.
//! - `histogram_sketch/one_series`: every thread records into one sketch.
//! - `histogram_raw/one_series` and `histogram_raw/series_per_thread`:
//!   raw-sample mode, where each observation is sent through the bounded queue
//!   instead of into a sketch. The worker drains the queue every 100 ms while
//!   the bench runs, the queue is sized so it never fills, and the bench
//!   asserts that no sample was dropped, so only accepted sends are timed.
#![allow(deprecated)]
use std::borrow::Cow;
use std::sync::Barrier;
use std::time::{Duration, Instant};

use chalk_metrics::client::{self, MetricsClient};
use criterion::measurement::WallTime;
use criterion::{
    BenchmarkGroup, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};

const OPS_PER_ITER: u64 = 1_000;
const THREADS: [usize; 5] = [1, 2, 4, 8, 16];
const POOL_SIZE: usize = 80;
const LATENCY_TABLE_LEN: usize = 4_096;

fn sketch_client() -> MetricsClient {
    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .build_local()
}

/// Raw-sample mode: no sketch, every observation goes through the bounded
/// queue. No exporter is registered, so the worker drains and discards the
/// queue every 100 ms. The capacity is far above what accumulates between two
/// drains, so the queue never fills.
fn raw_client() -> MetricsClient {
    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .aggregate_distr_metrics(false)
        .raw_sample_queue_capacity(1 << 26)
        .build_local()
}

fn static_tags() -> Vec<(&'static str, Cow<'static, str>)> {
    vec![
        ("endpoint", Cow::Borrowed("api/v1")),
        ("status", Cow::Borrowed("success")),
    ]
}

/// The 80 tag combinations of `metrics_throughput.rs`: 16 shards x 5 endpoints,
/// as `(tags_hash, endpoint)`.
fn pool_80() -> Vec<(u64, &'static str)> {
    use std::hash::{Hash, Hasher};
    let endpoints = ["api/v1", "api/v2", "health", "metrics", "graphql"];
    let mut pool = Vec::with_capacity(POOL_SIZE);
    for shard in 0..16u64 {
        for endpoint in endpoints {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            shard.hash(&mut h);
            endpoint.hash(&mut h);
            pool.push((h.finish(), endpoint));
        }
    }
    pool
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

/// Runs `op(thread, i)` for `i in 0..iters * OPS_PER_ITER` on each of `threads`
/// threads released together by a barrier. Every thread times its own loop;
/// the slowest thread's time is returned, so spawn and join are excluded.
fn run_threads<F>(threads: usize, iters: u64, op: &F) -> Duration
where
    F: Fn(usize, u64) + Sync,
{
    let ops = iters * OPS_PER_ITER;
    let barrier = Barrier::new(threads);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let barrier = &barrier;
                s.spawn(move || {
                    barrier.wait();
                    let start = Instant::now();
                    for i in 0..ops {
                        op(t, i);
                    }
                    start.elapsed()
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .max()
            .unwrap()
    })
}

/// Benchmarks `op` at every thread count, then shuts `client` down. `client` is
/// the one `op` records into.
fn bench_scenario<F>(
    group: &mut BenchmarkGroup<'_, WallTime>,
    scenario: &str,
    client: &MetricsClient,
    op: F,
) where
    F: Fn(usize, u64) + Sync,
{
    for threads in THREADS {
        group.throughput(Throughput::Elements(OPS_PER_ITER * threads as u64));
        group.bench_function(
            BenchmarkId::new(scenario, format!("{threads}_threads")),
            |b| b.iter_custom(|iters| run_threads(threads, iters, &op)),
        );
        // Raw-sample scenarios must time accepted sends only. Clients without a
        // raw queue always report zero.
        assert_eq!(
            client.dropped_raw_samples(),
            0,
            "{scenario} at {threads} threads: the raw-sample queue filled"
        );
    }
    client.shutdown();
}

fn thread_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("thread_scaling");
    let pool = pool_80();
    let latencies = latency_table();
    let latency = |i: u64| latencies[i as usize & (LATENCY_TABLE_LEN - 1)];

    let client = sketch_client();
    bench_scenario(&mut group, "count/one_series", &client, |_, _| {
        client.record_count("request_count", &["http"], 42, static_tags, 1);
    });

    // The stripe is chosen from the low 6 bits of the combined hash, which is
    // `tags_hash * odd constant + name hash`. Multiplying by an odd constant is
    // a bijection modulo 64, so the hashes 0..16 land on 16 different stripes:
    // each thread takes its own lock and updates its own counter.
    let client = sketch_client();
    bench_scenario(&mut group, "count/series_per_thread", &client, |t, _| {
        client.record_count("request_count", &["http"], t as u64, static_tags, 1);
    });

    let client = sketch_client();
    bench_scenario(&mut group, "count/pool_80", &client, |t, i| {
        let (hash, endpoint) = pool[(t * 31 + i as usize) % POOL_SIZE];
        let tags = || {
            vec![
                ("endpoint", Cow::Owned(endpoint.to_string())),
                ("status", Cow::Borrowed("success")),
            ]
        };
        client.record_count("request_count", &["http"], hash, tags, 1);
    });

    let client = sketch_client();
    bench_scenario(
        &mut group,
        "histogram_sketch/one_series",
        &client,
        |_, i| {
            client.record_histogram("request_latency", &["http"], 42, static_tags, latency(i));
        },
    );

    let client = raw_client();
    bench_scenario(&mut group, "histogram_raw/one_series", &client, |_, i| {
        client.record_histogram("request_latency", &["http"], 42, static_tags, latency(i));
    });

    // One series and one stripe per thread, as in `count/series_per_thread`;
    // the threads still share the raw-sample queue.
    let client = raw_client();
    bench_scenario(
        &mut group,
        "histogram_raw/series_per_thread",
        &client,
        |t, i| {
            let hash = t as u64;
            client.record_histogram("request_latency", &["http"], hash, static_tags, latency(i));
        },
    );

    group.finish();
}

criterion_group!(benches, thread_scaling);
criterion_main!(benches);
