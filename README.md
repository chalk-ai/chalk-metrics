# chalk-metrics

Efficient metrics aggregation with Rust macro definitions and pluggable exporters.

Define your metrics in Rust macro files, then record metrics with low overhead and cached tag data. Each metric is a struct — call `.record()` and the value type is enforced at compile time.

## Features

- **Type-safe recording**: Each metric is a struct with `.record(value)`. Count metrics accept `i64` (or no arg for +1), gauge/histogram accept `f64`. Wrong type = compile error.
- **Hierarchical namespaces**: Define namespace marker types, including optional parent namespaces. Exporters receive namespace segments and format them (Prometheus uses `_`, StatsD uses `.`).
- **Cached series tags**: Striped `parking_lot` locks with consistent `hashbrown::HashMap::entry` operations. Existing entries reuse tag data; raw observations use a bounded queue.
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

### Optional Python types

When the crate that invokes `define_tags!` and `define_metrics!` enables its
`python-bindings` feature and includes `pyo3` 0.30 as an optional dependency,
generated tags and metrics also become PyO3 classes. Enum tags retain their
Rust variants in Python, while free-form tags are constructed from their value:

```python
Status.Success
Endpoint("/api")
```

Metric classes accept the generated tag objects and retain their Rust recording
API. Count metrics expose `record()` and `record_value(value)`; gauges and
histograms expose `record(value)`:

```python
RequestCount(status=Status.Success, endpoint=Endpoint("/api")).record()
RequestDurationMs(status=Status.Success, endpoint=Endpoint("/api")).record(5.0)
```

A binding module still registers the generated classes with Python. That
registration is handled by the metric-binding layer.

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

`HistogramExportMode::Distribution` automatically requests each original sample;
it never substitutes a sketch's mean. Prometheus and StatsD `Percentiles` request
sketches. Registering both kinds records both representations, and each exporter
receives only its requested histogram input. Counts and gauges are exported once
per destination. Direct Distribution users must call `export_raw_points`;
passing histogram sketches to `export` rejects those sketches because original
samples cannot be reconstructed. Valid counters and gauges in the same batch are
still sent, and the call returns an error. Do not retry the whole batch blindly:
some metrics may already have been delivered.

Distribution export packs values with identical namespace, name, and tags into
DogStatsD protocol v1.1 messages, for example `latency:1:99|d`. Both observations
are preserved, including repeated values. Packed messages split at the datagram
byte limit. This requires Datadog Agent 6.25+ (6.x) or 7.25+; use
`.distribution_value_packing(false)` on the exporter for older agents.

Compared with the former mean-only default, correct Distribution export sends
one observation per recording call. Packing reduces repeated names and tags,
but payload size and agent processing still grow with sample rate. It does not
increase the number of distinct series when names and tags are unchanged.
StatsD `Percentiles` remains the default and uses local sketches; its per-process
percentiles cannot be merged into a global percentile. Sampling is not implicit.
Packing can add CPU without reducing bytes when each series has only one sample
per batch. It can also be disabled for these workloads. See the measured traffic,
latency, queue loss, and CPU tradeoffs in [distribution volume benchmarks](docs/distribution-volume.md).

### Migrating the global distribution toggle

Existing `init()`, `try_init()`, and `build_local()` calls remain valid. The global
`.aggregate_distr_metrics(...)` method is deprecated. For compatibility, legacy
`false` with a StatsD exporter whose histogram mode was never explicitly set
still emits original `name:v|d` observations (packed by default), preserving metric
names and types. Without that legacy setting, StatsD still defaults to Percentiles.

Before removing legacy `false`, explicitly select the equivalent exporter mode:

```rust
let exporter = StatsdExporter::udp("localhost:8125")
    .histogram_mode(HistogramExportMode::Distribution)
    .build()?;
// Register exporter and remove .aggregate_distr_metrics(false).
```

An explicit `histogram_mode(...)` always wins, including explicit Percentiles with
legacy `false`. Previously that combination emitted raw `name:v|d` observations;
it now emits `name.count`, `name.avg`, and `name.p50` (etc.) gauges. Select Distribution
to retain the old names and types. Prometheus always receives sketches, even with
legacy `false`, so mixed exporters retain both representations. The legacy setting
remains the fallback for custom exporters whose `distribution_input()` returns
`None`: `true` selects sketches and `false` selects raw points, as before.

This preserves source initialization APIs and the common legacy default-StatsD
configuration. Deliberate behavior changes remain: Distribution preserves every
observation instead of substituting a mean, explicit modes are honored, and direct
sketch-to-Distribution export returns an error. See [release notes](CHANGELOG.md).

Custom raw exporters should return `Some(DistributionInput::Raw)` from
`distribution_input()` and implement `export_raw_points`. Sketch exporters can
return `Some(DistributionInput::Sketch)`; `Both` explicitly requests both callbacks.

### Custom Exporters

Exporters run on the worker's tokio runtime. chalk-metrics does not enable tokio's
`net` feature, so an exporter that uses `tokio::net` must enable it in its own crate.

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
    .raw_sample_queue_capacity(16_384) // queued original observations
    .raw_sample_flush_interval(Duration::from_millis(100))
    .init();                         // or .try_init() for Result
```

### Flush latency, overflow, and shutdown

Counters, gauges, and sketches are exported on `flush_interval` (default 10
seconds). Original observations use a separate `raw_sample_flush_interval`
(default 100 milliseconds, must be positive). Raw observations are exported
one raw flush interval after the previous export, or sooner once the queue is a
quarter full (checked ten times per interval, but no more often than once per
millisecond). Both timers skip their
immediate startup tick. Allow up to the relevant interval plus scheduling and exporter
execution time. Slow exporters can extend either latency because calls are
sequential, even with multiple runtime threads. A custom `Both` exporter receives
independent raw and aggregate windows. Increasing `worker_threads` does not
parallelize exporter calls.

The shared raw-sample queue defaults to 16,384 observations and can be sized with
`raw_sample_queue_capacity(n)` (`n > 0`). Size it for peak sample rate times the
raw interval plus slow-exporter headroom and bursts. Packing reduces wire bytes,
not queue occupancy: each observation still occupies one slot. Recording never
waits for space:
overflow drops the newest raw sample. `client.dropped_raw_samples()` (local clients) or
`chalk_metrics::client::dropped_raw_samples()` (global client) counts full or
closed queue rejections. Sketch consumers still include these observations.
This counter does not include network failures.

The StatsD exporter never waits on the socket. A datagram the socket does not
accept (for example a full receiver queue) is dropped, and the export call
returns an error that the worker prints at most once per second per kind with a
count of suppressed errors. Call `StatsdExporter::drop_counters()` before handing
the exporter to the client: the returned `StatsdDropCounters` reports
`datagrams()`, `lines()` and `bytes()` dropped, including single lines rejected
for exceeding `max_buffer_size` (counted in lines and bytes only). Datagrams are
sent while later lines are still being formatted, which keeps bursts small.

UDP accepts IPv4, bracketed IPv6, and hostnames with ports. Dual-stack hostnames
prefer IPv4, preserving `localhost` delivery to IPv4-only agents. Explicit IPv6
and IPv6-only hostnames use IPv6. Address selection cannot detect which UDP
receiver is listening. Datagram limits are 1432 bytes for UDP and 8192 for UDS by default, configurable with
`max_buffer_size(n)` (`n > 0`). Limits include UTF-8 bytes and newline separators.
A line larger than the limit is rejected without splitting it; valid sibling
lines are sent and the batch returns an error. Send failures are returned to
direct callers and logged by the worker. A failed send does not prevent attempts
to send later datagrams in the batch; earlier successes are not replayed. A batch
error can therefore mean partial delivery. There are no delivery acknowledgments
or retries, and successful UDP sends do not guarantee receiver delivery. UDS
connections retain their socket on `WouldBlock` (receiver backpressure); that
send fails without a retry. Other send failures clear the connection so a later
datagram can reconnect. Socket replacement also triggers reconnection.

Stop and join metric-producing threads, then call the local client's
`shutdown()` or global `chalk_metrics::client::shutdown()`. Shutdown closes the
raw queue, exports pending aggregates and accepted raw samples, and joins the
worker; it is idempotent and blocks on exporter completion. Do not call it from
an exporter callback. Local clients require explicit shutdown. The global client
also registers an `atexit` handler, but abrupt termination, signals, and process
aborts can bypass it. Recording after shutdown is unsupported.

Tenant identity, required service tags, and service-specific tag validation
remain the consumer's responsibility; the shared client does not infer them.

## Benchmarks

Run benchmarks with:

```bash
cargo bench
```

[Goalpost benchmarks](docs/goalposts.md) lists every bench target, the number
each one tracks, and the current value: the generated recording API by tag
shape, thread scaling, flush cost and stalls, raw-sample and socket delivery,
export cost by metric kind, wire bytes per observation, and macro compile time.
Allocations per recording call are enforced by `tests/alloc_budget.rs`.

Before/after results, reproduction commands, workload definitions, and the
measurement environment are recorded in [adoption benchmarks](docs/adoption-benchmarks.md).
The latest [stack review results](docs/stack-review.md) compare upstream, the
published stack, and this revision, including the name-hashing hypothesis and
regression evidence.
[Machine-readable estimates](docs/benchmark-results.json) include the upstream
baseline and each implementation stage.

The [distribution volume results](docs/distribution-volume.md) also compare the
original upstream, the first published stack, and the revised stack with the
same harness. They include payload bytes, datagrams, twelve-second paced loads,
queue drops, latency, and process CPU; all received values are decoded.

The report covers existing-series updates, forced map growth, contention,
accepted raw-sample recording, real UDP export, and Prometheus rendering.
Distribution benchmarks count individual observations. Growth timings are
reported alongside the baseline's duplicate-entry failures, and the measured
counter/gauge overhead is disclosed explicitly.

## Releasing

Releases are automated with [release-plz](https://release-plz.dev). On every push
to `main` that changes either crate, it opens or updates a release PR that bumps
`chalk-metrics` and `chalk-metrics-macros` to the same next version and adds a
CHANGELOG section generated from commit messages. Edit the PR's CHANGELOG if
needed (release-plz rewrites the PR on every push to `main`, so edit just before
merging), then merge it once its checks pass. Merging publishes
`chalk-metrics-macros` and then `chalk-metrics` to crates.io, tags `v<version>`,
and creates the GitHub release. Other merges to `main` never publish (`release_always = false` in
`release-plz.toml`).

CI runs on pull requests only, including release PRs, so the release PR's checks
test exactly what gets published. Branch protection on `main` requires the `test`,
`msrv`, `audit`, and `package` checks before any PR, including a release PR, can be
merged. The repository must allow GitHub Actions to create pull requests (Settings >
Actions > General > Workflow permissions).
