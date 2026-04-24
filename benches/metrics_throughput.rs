use std::borrow::Cow;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use chalk_metrics::client;

const OPS_PER_ITER: u64 = 1_000;
const CONTENTION_OPS: u64 = 16_000;

fn make_local_client() -> client::MetricsClient {
    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .build_local()
}

/// 80 tag combinations: 16 shards x 5 endpoints
fn mixed_tags_pool() -> Vec<(
    u64,
    Box<dyn Fn() -> Vec<(&'static str, Cow<'static, str>)> + Send + Sync>,
)> {
    let endpoints = ["api/v1", "api/v2", "health", "metrics", "graphql"];
    let mut pool: Vec<(
        u64,
        Box<dyn Fn() -> Vec<(&'static str, Cow<'static, str>)> + Send + Sync>,
    )> = Vec::new();

    for shard in 0..16u64 {
        for &ep in &endpoints {
            let ep = ep.to_string();
            let hash = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                shard.hash(&mut h);
                ep.hash(&mut h);
                h.finish()
            };
            pool.push((
                hash,
                Box::new(move || {
                    vec![
                        ("endpoint", Cow::Owned(ep.clone())),
                        ("status", Cow::Borrowed("success")),
                    ]
                }),
            ));
        }
    }
    pool
}

fn run_contended<F>(num_threads: usize, iters: u64, op: Arc<F>) -> Duration
where
    F: Fn(usize, usize) + Send + Sync + 'static,
{
    let ops_per_thread = CONTENTION_OPS as usize / num_threads;
    let mut total = Duration::ZERO;

    for _ in 0..iters {
        let barrier = Arc::new(Barrier::new(num_threads + 1));
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let barrier = Arc::clone(&barrier);
                let op = Arc::clone(&op);
                let done = Arc::clone(&done);
                std::thread::spawn(move || {
                    barrier.wait();
                    for i in 0..ops_per_thread {
                        op(t, i);
                    }
                    done.fetch_add(1, std::sync::atomic::Ordering::Release);
                })
            })
            .collect();

        barrier.wait();
        let start = Instant::now();

        for h in handles {
            h.join().unwrap();
        }

        total += start.elapsed();
    }

    total
}

fn single_thread_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_thread");
    group.throughput(Throughput::Elements(OPS_PER_ITER));

    group.bench_function("count/static_tags", |b| {
        let local = make_local_client();
        b.iter(|| {
            for _ in 0..OPS_PER_ITER {
                local.record_count(
                    "request_count",
                    &["http"],
                    42,
                    || {
                        vec![
                            ("endpoint", Cow::Borrowed("api/v1")),
                            ("status", Cow::Borrowed("success")),
                        ]
                    },
                    1,
                );
            }
        });
        local.flush();
        local.shutdown();
    });

    group.bench_function("count/mixed_tags", |b| {
        let local = make_local_client();
        let pool = mixed_tags_pool();
        b.iter(|| {
            for i in 0..OPS_PER_ITER as usize {
                let (hash, make_tags) = &pool[i % pool.len()];
                local.record_count("request_count", &["http"], *hash, make_tags, 1);
            }
        });
        local.flush();
        local.shutdown();
    });

    group.bench_function("gauge/static_tags", |b| {
        let local = make_local_client();
        b.iter(|| {
            for _ in 0..OPS_PER_ITER {
                local.record_gauge(
                    "active_connections",
                    &["http"],
                    42,
                    || vec![("endpoint", Cow::Borrowed("api/v1"))],
                    100.0,
                );
            }
        });
        local.flush();
        local.shutdown();
    });

    group.bench_function("gauge/mixed_tags", |b| {
        let local = make_local_client();
        let pool = mixed_tags_pool();
        b.iter(|| {
            for i in 0..OPS_PER_ITER as usize {
                let (hash, make_tags) = &pool[i % pool.len()];
                local.record_gauge("active_connections", &["http"], *hash, make_tags, 100.0);
            }
        });
        local.flush();
        local.shutdown();
    });

    group.bench_function("histogram/static_tags", |b| {
        let local = make_local_client();
        b.iter(|| {
            for i in 0..OPS_PER_ITER {
                local.record_histogram(
                    "request_latency",
                    &["http"],
                    42,
                    || {
                        vec![
                            ("endpoint", Cow::Borrowed("api/v1")),
                            ("status", Cow::Borrowed("success")),
                        ]
                    },
                    i as f64 * 0.001,
                );
            }
        });
        local.flush();
        local.shutdown();
    });

    group.bench_function("histogram/mixed_tags", |b| {
        let local = make_local_client();
        let pool = mixed_tags_pool();
        b.iter(|| {
            for i in 0..OPS_PER_ITER as usize {
                let (hash, make_tags) = &pool[i % pool.len()];
                local.record_histogram(
                    "request_latency",
                    &["http"],
                    *hash,
                    make_tags,
                    i as f64 * 0.001,
                );
            }
        });
        local.flush();
        local.shutdown();
    });

    group.finish();
}

fn contention_benchmarks(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    group.throughput(Throughput::Elements(CONTENTION_OPS));

    for num_threads in [2, 8] {
        group.bench_with_input(
            BenchmarkId::new("count/mixed", format!("{num_threads}_threads")),
            &num_threads,
            |b, &num_threads| {
                let local = Arc::new(make_local_client());
                let pool = Arc::new(mixed_tags_pool());

                b.iter_custom(|iters| {
                    let op = {
                        let local = Arc::clone(&local);
                        let pool = Arc::clone(&pool);
                        Arc::new(move |t: usize, i: usize| {
                            let ops_per_thread = CONTENTION_OPS as usize / num_threads;
                            let idx = (t * ops_per_thread + i) % pool.len();
                            let (hash, make_tags) = &pool[idx];
                            local.record_count("request_count", &["http"], *hash, make_tags, 1);
                        })
                    };
                    let d = run_contended(num_threads, iters, op);
                    local.flush();
                    d
                });

                local.shutdown();
            },
        );

        group.bench_with_input(
            BenchmarkId::new("histogram/mixed", format!("{num_threads}_threads")),
            &num_threads,
            |b, &num_threads| {
                let local = Arc::new(make_local_client());
                let pool = Arc::new(mixed_tags_pool());

                b.iter_custom(|iters| {
                    let op = {
                        let local = Arc::clone(&local);
                        let pool = Arc::clone(&pool);
                        Arc::new(move |t: usize, i: usize| {
                            let ops_per_thread = CONTENTION_OPS as usize / num_threads;
                            let idx = (t * ops_per_thread + i) % pool.len();
                            let (hash, make_tags) = &pool[idx];
                            local.record_histogram(
                                "request_latency",
                                &["http"],
                                *hash,
                                make_tags,
                                i as f64 * 0.001,
                            );
                        })
                    };
                    let d = run_contended(num_threads, iters, op);
                    local.flush();
                    d
                });

                local.shutdown();
            },
        );
    }

    group.finish();
}

criterion_group!(benches, single_thread_benchmarks, contention_benchmarks);
criterion_main!(benches);
