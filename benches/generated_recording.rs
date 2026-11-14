//! Recording cost of macro-generated metrics the way callers use them: real
//! tags, including `String` tags built on every call as the README shows
//! (`Endpoint::from("/api")`).
//!
//! The other recording benches pass a precomputed tag hash or use zero-tag
//! metrics, so they skip building and hashing tags.
//!
//! Case names:
//! - `built`: the metric struct is constructed inside the loop from
//!   `black_box(&str)` values, so every call pays for its `String` tags.
//! - `borrowed`: the metric's `<Metric>Ref` struct, constructed inside the loop
//!   from the same `black_box(&str)` values; no `String` is built.
//! - `prebuilt`: the struct is constructed once outside the loop and each call
//!   is `.record()` on `black_box(&metric)`.
//! - `uninitialized/*` run before the global client is initialized: what a
//!   disabled client costs.
//!
//! Each iteration is 1,000 calls. No exporter is registered, so histograms
//! aggregate into sketches.
use std::hint::black_box;
use std::time::Duration;

use chalk_metrics::{client, define_metrics, define_namespaces, define_tags};
use criterion::measurement::WallTime;
use criterion::{BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main};

define_tags! {
    Status => "status" {
        Success => "success",
        Failure => "failure",
    }
    Method => "method" {
        Get => "get",
        Post => "post",
    }
    Endpoint => "endpoint";
    Region => "region";
    Tenant => "tenant";
}

define_namespaces! {
    Http => "http";
}

define_metrics! {
    group(tags = []) {
        count Count0Tags => "count_0_tags", "No tags";
        histogram Histogram0Tags => "histogram_0_tags", "No tags";
    }
    group(namespace = Http, tags = [Status]) {
        count Count1Enum => "count_1_enum", "One enum tag";
    }
    group(namespace = Http, tags = [Status, Method]) {
        count Count2Enum => "count_2_enum", "Two enum tags";
    }
    group(namespace = Http, tags = [Endpoint, Status]) {
        count CountStringEnum => "count_string_enum", "String and enum tags";
        gauge GaugeStringEnum => "gauge_string_enum", "String and enum tags";
        histogram HistogramStringEnum => "histogram_string_enum", "String and enum tags";
    }
    group(namespace = Http, tags = [Endpoint, Region, Tenant, Status, Method]) {
        count Count5Tags => "count_5_tags", "Three string and two enum tags";
        histogram Histogram5Tags => "histogram_5_tags", "Three string and two enum tags";
    }
}

const CALLS_PER_ITER: u64 = 1000;

const ENDPOINT: &str = "/api/v1/users";
/// 57 bytes: a fully qualified resolver name.
const LONG_ENDPOINT: &str = "chalk.resolvers.user_features.get_transaction_velocity_7d";
const REGION: &str = "us-east-1";
const TENANT: &str = "tenant-0042";

const LATENCY_TABLE_LEN: usize = 4096;

/// xorshift64* generator: deterministic, no dependencies.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
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

    /// Lognormal with the given median and sigma (of the underlying normal).
    fn lognormal(&mut self, median: f64, sigma: f64) -> f64 {
        (median.ln() + sigma * self.normal()).exp()
    }
}

/// Request-latency-like values in seconds (median 20 ms, sigma 0.8), cycled
/// across iterations so every histogram case sees the same value sequence.
struct Latencies {
    table: [f64; LATENCY_TABLE_LEN],
    cursor: usize,
}

impl Latencies {
    fn new() -> Self {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        Self {
            table: std::array::from_fn(|_| rng.lognormal(0.020, 0.8)),
            cursor: 0,
        }
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let value = self.table[self.cursor % LATENCY_TABLE_LEN];
        self.cursor = self.cursor.wrapping_add(1);
        value
    }
}

fn count_5_tags() -> Count5Tags {
    Count5Tags {
        endpoint: Endpoint::from(black_box(ENDPOINT)),
        region: Region::from(black_box(REGION)),
        tenant: Tenant::from(black_box(TENANT)),
        status: Status::Success,
        method: Method::Post,
    }
}

fn histogram_5_tags() -> Histogram5Tags {
    Histogram5Tags {
        endpoint: Endpoint::from(black_box(ENDPOINT)),
        region: Region::from(black_box(REGION)),
        tenant: Tenant::from(black_box(TENANT)),
        status: Status::Success,
        method: Method::Post,
    }
}

fn count_5_tags_borrowed() -> Count5TagsRef<'static> {
    Count5TagsRef {
        endpoint: black_box(ENDPOINT),
        region: black_box(REGION),
        tenant: black_box(TENANT),
        status: Status::Success,
        method: Method::Post,
    }
}

fn histogram_5_tags_borrowed() -> Histogram5TagsRef<'static> {
    Histogram5TagsRef {
        endpoint: black_box(ENDPOINT),
        region: black_box(REGION),
        tenant: black_box(TENANT),
        status: Status::Success,
        method: Method::Post,
    }
}

fn bench_calls(group: &mut BenchmarkGroup<'_, WallTime>, name: &str, mut call: impl FnMut()) {
    group.bench_function(name, |b| {
        b.iter(|| {
            for _ in 0..CALLS_PER_ITER {
                call();
            }
        })
    });
}

fn generated_recording(c: &mut Criterion) {
    let mut group = c.benchmark_group("generated_recording");
    group.throughput(Throughput::Elements(CALLS_PER_ITER));

    // Before `init()`: recording is a no-op, but the caller still builds and
    // hashes the tags.
    bench_calls(&mut group, "uninitialized/count/0_tags", || {
        Count0Tags {}.record()
    });
    bench_calls(&mut group, "uninitialized/count/string_enum_built", || {
        CountStringEnum {
            endpoint: Endpoint::from(black_box(ENDPOINT)),
            status: Status::Success,
        }
        .record()
    });
    bench_calls(
        &mut group,
        "uninitialized/count/string_enum_borrowed",
        || {
            CountStringEnumRef {
                endpoint: black_box(ENDPOINT),
                status: Status::Success,
            }
            .record()
        },
    );

    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .init();

    bench_calls(&mut group, "count/0_tags", || Count0Tags {}.record());
    bench_calls(&mut group, "count/1_enum", || {
        Count1Enum {
            status: black_box(Status::Success),
        }
        .record()
    });
    bench_calls(&mut group, "count/2_enum", || {
        Count2Enum {
            status: black_box(Status::Failure),
            method: black_box(Method::Get),
        }
        .record()
    });
    let prebuilt = CountStringEnum {
        endpoint: Endpoint::from(ENDPOINT),
        status: Status::Success,
    };
    bench_calls(&mut group, "count/string_enum_prebuilt", || {
        black_box(&prebuilt).record()
    });
    bench_calls(&mut group, "count/string_enum_built", || {
        CountStringEnum {
            endpoint: Endpoint::from(black_box(ENDPOINT)),
            status: Status::Success,
        }
        .record()
    });
    bench_calls(&mut group, "count/long_string_enum_built", || {
        CountStringEnum {
            endpoint: Endpoint::from(black_box(LONG_ENDPOINT)),
            status: Status::Success,
        }
        .record()
    });
    bench_calls(&mut group, "count/string_enum_borrowed", || {
        CountStringEnumRef {
            endpoint: black_box(ENDPOINT),
            status: Status::Success,
        }
        .record()
    });
    let prebuilt_5_tags = count_5_tags();
    bench_calls(&mut group, "count/5_tags_prebuilt", || {
        black_box(&prebuilt_5_tags).record()
    });
    bench_calls(&mut group, "count/5_tags_built", || count_5_tags().record());
    bench_calls(&mut group, "count/5_tags_borrowed", || {
        count_5_tags_borrowed().record()
    });
    bench_calls(&mut group, "gauge/string_enum_built", || {
        GaugeStringEnum {
            endpoint: Endpoint::from(black_box(ENDPOINT)),
            status: Status::Success,
        }
        .record(black_box(42.0))
    });
    bench_calls(&mut group, "gauge/string_enum_borrowed", || {
        GaugeStringEnumRef {
            endpoint: black_box(ENDPOINT),
            status: Status::Success,
        }
        .record(black_box(42.0))
    });

    let mut latencies = Latencies::new();
    bench_calls(&mut group, "histogram/0_tags", || {
        Histogram0Tags {}.record(latencies.next())
    });
    latencies.cursor = 0;
    bench_calls(&mut group, "histogram/string_enum_built", || {
        HistogramStringEnum {
            endpoint: Endpoint::from(black_box(ENDPOINT)),
            status: Status::Success,
        }
        .record(latencies.next())
    });
    latencies.cursor = 0;
    bench_calls(&mut group, "histogram/5_tags_built", || {
        histogram_5_tags().record(latencies.next())
    });
    latencies.cursor = 0;
    bench_calls(&mut group, "histogram/string_enum_borrowed", || {
        HistogramStringEnumRef {
            endpoint: black_box(ENDPOINT),
            status: Status::Success,
        }
        .record(latencies.next())
    });
    latencies.cursor = 0;
    bench_calls(&mut group, "histogram/5_tags_borrowed", || {
        histogram_5_tags_borrowed().record(latencies.next())
    });

    group.finish();
    client::shutdown();
}

criterion_group!(benches, generated_recording);
criterion_main!(benches);
