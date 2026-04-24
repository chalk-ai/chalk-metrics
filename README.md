# chalk-metrics

Efficient metrics aggregation with Rust macro definitions and pluggable exporters.

Define your metrics in Rust macro files, then record metrics with zero-allocation hot-path performance. Each metric is a struct — call `.record()` and the value type is enforced at compile time.

## Features

- **Type-safe recording**: Each metric is a struct with `.record(value)`. Count metrics accept `i64` (or no arg for +1), gauge/histogram accept `f64`. Wrong type = compile error.
- **Hierarchical namespaces**: Define namespace marker types, including optional parent namespaces. Exporters receive namespace segments and format them (Prometheus uses `_`, StatsD uses `.`).
- **Zero-allocation hot path**: Striped `parking_lot` locks with `hashbrown::RawEntryMut` — only allocates on first-seen tag combinations.
- **Pluggable exporters**: Prometheus text format and StatsD/DogStatsD included, plus an async `Exporter` trait for custom backends.
- **Singleton client**: `env_logger`-style `init()`/`try_init()` with automatic shutdown via `atexit`.

## Quick Start

### 1. Define metrics in Rust files

```rust
// src/metrics/tags.rs
chalk_metrics::define_tags! {
    pub Status => "status" {
        Success => "success",
        Failure => "failure",
    }

    pub Endpoint => "endpoint";
    pub Region => "region";
}
```

```rust
// src/metrics/namespaces.rs
chalk_metrics::define_namespaces! {
    pub Http => "http";
    pub Resolver => "resolver";
    pub HttpAuth(parent = Http) => "auth";
}
```

```rust
// src/metrics/definitions.rs
use super::{Endpoint, Http, Region, Resolver, Status};

chalk_metrics::define_metrics! {
    group(namespace = Http, tags = [Endpoint, Status]) {
        pub count HttpRequestCount => "request_count", "Total HTTP requests";
        pub histogram HttpRequestLatency => "request_latency", "HTTP request latency in seconds";
    }

    group(tags = []) {
        pub gauge Uptime => "uptime", "Process uptime in seconds";
    }

    group(namespace = Http, tags = [Endpoint]) {
        pub gauge HttpActiveConnections => "active_connections",
            "Current active connections",
            tags += [
                optional Region,
            ];
    }

    group(namespace = Resolver, tags = [Endpoint]) {
        pub histogram ResolverLatency => "latency",
            "Resolver execution latency",
            tags += [
                // Uses Status values, but the generated field is
                // `resolver_status` and exporters receive key "resolver_status".
                Status as resolver_status,

                // Optional alias: generated field is Option<Endpoint>;
                // exporters receive key "resolver_fqn" only when Some(...).
                optional Endpoint as resolver_fqn,
            ];
    }
}
```

```rust
// src/metrics/mod.rs
mod tags;
mod namespaces;
mod definitions;

pub use definitions::*;
pub use namespaces::*;
pub use tags::*;
```

### 2. Initialize and record

```rust
use chalk_metrics::export::prometheus::PrometheusExporter;
use crate::metrics::*;
use std::time::Duration;

fn main() {
    let prom = PrometheusExporter::builder().namespace("myapp").build();

    chalk_metrics::client::builder()
        .with_exporter(prom)
        .flush_interval(Duration::from_secs(10))
        .init();

    // Count — increment by 1 (no arg)
    HttpRequestCount {
        endpoint: Endpoint::from("/api"),
        status: Status::Success,
    }.record();

    // Count — explicit delta
    HttpRequestCount {
        endpoint: Endpoint::from("/api"),
        status: Status::Failure,
    }.record_value(5);

    // Histogram — f64 value
    HttpRequestLatency {
        endpoint: Endpoint::from("/api"),
        status: Status::Success,
    }.record(0.042);

    // Gauge — f64 value (top-level, no namespace)
    Uptime {}.record(42.0);

    // Optional tags are Option<TagType>
    HttpActiveConnections {
        endpoint: Endpoint::from("/api"),
        region: Some(Region::from("us-east-1")),
    }.record(12.0);

    HttpActiveConnections {
        endpoint: Endpoint::from("/api"),
        region: None,
    }.record(8.0);

    // Aliased tags use the alias as the struct field and export key
    ResolverLatency {
        endpoint: Endpoint::from("my.resolver"),
        resolver_status: Status::Success,
        resolver_fqn: Some(Endpoint::from("my.resolver.fqn")),
    }.record(0.005);
}
```

## Macro Reference

### Tags

Tags are defined globally and reused across metrics.

```rust
chalk_metrics::define_tags! {
    // Enum tag: variants are type checked and export borrowed strings.
    pub Status => "status" {
        Success => "success",
        Failure => "failure",
    }

    // Free-form string tag: accepts Endpoint::from(&str) or Endpoint::from(String).
    pub Endpoint => "endpoint";
}
```

### Namespaces

Metrics can be organized with namespace marker types. Parent namespaces can be declared in the same or a separate `define_namespaces!` invocation as long as the parent type is in scope.

```rust
chalk_metrics::define_namespaces! {
    pub Http => "http";
}

chalk_metrics::define_namespaces! {
    pub HttpAuth(parent = Http) => "auth";
}
```

- Metrics under `http` get namespace `["http"]`
- Metrics under `HttpAuth` get namespace `["http", "auth"]`
- Top-level metrics get namespace `[]`

### Metrics

```rust
chalk_metrics::define_metrics! {
    group(namespace = Http, tags = [Endpoint, Status]) {
        pub count HttpRequestCount => "request_count", "Total HTTP requests";
        pub histogram HttpRequestLatency => "request_latency", "HTTP request latency";
    }

    group(tags = []) {
        pub gauge Uptime => "uptime", "Process uptime in seconds";
    }
}
```

- Metric types are `count`, `gauge`, and `histogram`.
- Group tags are inherited by every metric in the group.
- Additional per-metric tags are added with `tags += [...]`.
- Multiple macro calls are supported, so metrics can be split across files or modules.
- Duplicate Rust type names are caught by Rust. Duplicate exported metric names across separate macro calls are the user's responsibility.

Optional tags are declared with `optional TagType`. The generated struct field is `Option<TagType>`, and the tag is omitted from exports when the value is `None`.

```rust
chalk_metrics::define_metrics! {
    group(namespace = Http, tags = [Endpoint]) {
        pub gauge HttpActiveConnections => "active_connections",
            "Current active connections",
            tags += [
                optional Region,
            ];
    }
}
```

Tag aliases are declared with `TagType as field_name`. The alias changes the generated field name and exported tag key for that metric only. The tag type and allowed values stay the same.

```rust
chalk_metrics::define_metrics! {
    group(namespace = Resolver, tags = [Endpoint]) {
        pub histogram ResolverLatency => "latency",
            "Resolver execution latency",
            tags += [
                Status as resolver_status,
                optional Endpoint as resolver_fqn,
            ];
    }
}
```

Generated metric structs expose `NAME`, `namespace()`, `export_pairs()`, and type-specific recording methods. Count metrics support `.record()` and `.record_value(i64)`. Gauge and histogram metrics support `.record(f64)`.

## Migrating From JSON Codegen

- Remove the `build.rs` call to `chalk_metrics::codegen::generate(...)`.
- Remove `metrics.json`.
- Move tag, namespace, and metric definitions into Rust modules using the macros above.
- Replace `include!(concat!(env!("OUT_DIR"), "/metrics_generated.rs"))` with normal Rust modules and `pub use` re-exports.
- `chalk-metrics` is only needed in `[dependencies]`; no `[build-dependencies]` entry is required.

## Exporters

### Prometheus

Format-only exporter — renders metrics in Prometheus text exposition format.

```rust
let prom = PrometheusExporter::builder()
    .namespace("myapp")
    .bucket_boundaries(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0])
    .build();

// After flush, read the text output:
let text = prom.get_metrics_text();
```

Namespace segments are joined with `_`: `myapp_http_request_count_total`.

### StatsD / DogStatsD

```rust
// UDP
let statsd = StatsdExporter::udp("127.0.0.1:8125")
    .namespace("myapp")
    .histogram_mode(HistogramExportMode::Percentiles(vec![0.5, 0.9, 0.95, 0.99]))
    .build()?;

// Unix domain socket (with automatic reconnection)
let statsd = StatsdExporter::uds("/var/run/datadog/dsd.socket")
    .namespace("myapp")
    .histogram_mode(HistogramExportMode::Distribution)
    .build()?;
```

Namespace segments are joined with `.`: `myapp.http.request_count`.

### Custom Exporters

```rust
#[async_trait::async_trait]
impl Exporter for MyExporter {
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        for m in metrics {
            // m.namespace: &[&str]  — e.g., ["http", "auth"]
            // m.metric_name: &str   — e.g., "request_count"
            // m.tags.pairs: Vec<(&str, Cow<'static, str>)>
            //   enum tags: Cow::Borrowed (zero alloc)
            //   string tags: Cow::Owned
            // m.value: FlushedValue
        }
        Ok(())
    }
}
```

## Client Configuration

```rust
chalk_metrics::client::builder()
    .with_exporter(exporter1)
    .with_exporter(exporter2)       // multiple exporters supported
    .flush_interval(Duration::from_secs(10))
    .worker_threads(1)               // tokio runtime threads (default: 1)
    .max_buckets(200)                // histogram max buckets
    .initial_error(0.001)            // histogram error bound
    .init();                         // or .try_init() for Result
```

Shutdown happens automatically via `atexit`, or explicitly via `chalk_metrics::client::shutdown()`.

## Benchmarks

Run benchmarks with:

```bash
cargo bench
```

### Results

Measured on Apple M3 Max, 36 GB RAM, macOS Darwin 25.0.0 arm64, Rust 1.94.0.

#### Aggregation Throughput (1,000 ops/iter)

| Benchmark | Time/iter | Throughput |
|---|---|---|
| count/static_tags | 2.6 µs | ~388 M ops/s |
| count/mixed_tags (80 combos) | 2.8 µs | ~366 M ops/s |
| gauge/static_tags | 2.6 µs | ~390 M ops/s |
| gauge/mixed_tags | 2.8 µs | ~365 M ops/s |
| histogram/static_tags | 39 µs | ~25 M ops/s |
| histogram/mixed_tags | 43 µs | ~23 M ops/s |

#### Multi-Thread Contention (16,000 ops/iter)

| Benchmark | Time/iter | Throughput |
|---|---|---|
| count/mixed, 2 threads | 376 µs | ~43 M ops/s |
| count/mixed, 8 threads | 604 µs | ~26 M ops/s |
| histogram/mixed, 2 threads | 1.1 ms | ~15 M ops/s |
| histogram/mixed, 8 threads | 981 µs | ~16 M ops/s |

#### Prometheus Export (mixed count/gauge/histogram)

| Batch Size | 2 tags | 5 tags | 0 tags |
|---|---|---|---|
| 10 metrics | 53 µs | 55 µs | 48 µs |
| 100 metrics | 565 µs | 593 µs | 520 µs |
| 1,000 metrics | 5.7 ms | 6.0 ms | 5.4 ms |

#### StatsD Export (mixed count/gauge/histogram)

| Batch Size | Percentiles mode | Distribution mode |
|---|---|---|
| 10 metrics | 46 µs | 8 µs |
| 100 metrics | 510 µs | 64 µs |
| 1,000 metrics | 5.2 ms | 626 µs |

**Notes:**
- Count and gauge recording is ~2.6 µs per 1,000 ops (~2.6 ns/op) on the hot path thanks to lock-free atomics
- Histogram is slower (~39 ns/op) due to the parking_lot mutex protecting the UDD Sketch
- StatsD distribution mode is ~8x faster than percentiles mode because it emits 1 line per metric instead of 6 (count + avg + 4 percentiles)
- Mixed tags benchmarks rotate through 80 tag combinations to simulate realistic cardinality
