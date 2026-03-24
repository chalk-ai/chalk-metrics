# chalk-metrics

Efficient metrics aggregation with compile-time code generation and pluggable exporters.

Define your metrics in a JSON file, generate type-safe Rust code at build time, then record metrics with zero-allocation hot-path performance. Metrics are aggregated in-process using a 64-stripe concurrent map and exported on a configurable interval to one or more backends.

## Features

- **Compile-time code generation** from a JSON schema: type-safe metric IDs, tag structs, and enum tag values
- **Three metric types**: Count (up/down counter), Gauge (last-value-wins), Histogram (UDD Sketch with approximate quantiles)
- **Zero-allocation hot path**: striped `parking_lot` locks with `hashbrown::RawEntryMut` — only allocates on first-seen tag combinations
- **Pluggable exporters**: Prometheus text format and StatsD/DogStatsD included, plus an async `Exporter` trait for custom backends
- **Singleton client**: `env_logger`-style `init()`/`try_init()` with automatic shutdown via `atexit`
- **Drain-on-flush**: count and histogram entries are removed after each flush; gauge entries persist

## Quick Start

### 1. Define metrics in `metrics.json`

```json
{
    "tags": {
        "status": {
            "value_type": "enum",
            "values": ["success", "failure"],
            "export_name": "status"
        },
        "endpoint": {
            "value_type": "string",
            "export_name": "endpoint"
        }
    },
    "metrics": [
        {
            "name": "request_count",
            "type": "count",
            "tags": [
                { "tag": "endpoint" },
                { "tag": "status" }
            ],
            "description": "Total HTTP requests"
        },
        {
            "name": "request_latency",
            "type": "histogram",
            "tags": [
                { "tag": "endpoint" },
                { "tag": "status" }
            ],
            "description": "HTTP request latency in seconds"
        }
    ]
}
```

### 2. Set up `build.rs`

```rust
fn main() {
    chalk_metrics::codegen::generate("metrics.json");
}
```

Add `chalk-metrics` to both `[dependencies]` and `[build-dependencies]` in your `Cargo.toml`.

### 3. Include generated code

```rust
mod metrics {
    include!(concat!(env!("OUT_DIR"), "/metrics_generated.rs"));
}
```

### 4. Initialize and record

```rust
use chalk_metrics::export::prometheus::PrometheusExporter;
use std::time::Duration;

fn main() {
    let prom = PrometheusExporter::builder().namespace("myapp").build();
    let text_handle = prom.text_handle();

    chalk_metrics::client::builder()
        .with_exporter(prom)
        .flush_interval(Duration::from_secs(10))
        .init();

    // Record metrics using macros
    chalk_metrics::count!(RequestCount, 1, metrics::RequestCountTags {
        endpoint: metrics::Endpoint::from("/api"),
        status: metrics::Status::Success,
    });

    chalk_metrics::histogram!(RequestLatency, 0.042, metrics::RequestLatencyTags {
        endpoint: metrics::Endpoint::from("/api"),
        status: metrics::Status::Success,
    });

    // Read Prometheus output (serve at /metrics in your HTTP server)
    let output = text_handle.read().clone();
}
```

## JSON Schema Reference

### Tag Definitions

Tags are defined globally and reused across metrics.

| Field | Type | Description |
|---|---|---|
| `value_type` | `"enum"` or `"string"` | Whether values are constrained to a fixed set |
| `values` | `string[]` | Required for enum tags: the allowed values |
| `export_name` | `string` | Default key name used when exporting |

### Metric Definitions

| Field | Type | Description |
|---|---|---|
| `name` | `string` | Metric name (snake_case) |
| `type` | `"count"`, `"gauge"`, or `"histogram"` | Aggregation type |
| `tags` | `array` | Tag references |
| `description` | `string` | Human-readable description |

### Tag References (per-metric)

| Field | Type | Description |
|---|---|---|
| `tag` | `string` | Name of the tag definition |
| `export_name` | `string?` | Override the export key for this metric |
| `optional` | `bool` | Whether this tag can be omitted (default: false) |

## Exporters

### Prometheus

Format-only exporter — renders metrics in Prometheus text exposition format. Use `get_metrics_text()` or `text_handle()` to retrieve the output and serve it from your own HTTP server.

```rust
let prom = PrometheusExporter::builder()
    .namespace("myapp")
    .bucket_boundaries(vec![0.01, 0.05, 0.1, 0.5, 1.0, 5.0])
    .build();
```

### StatsD / DogStatsD

Sends metrics over UDP or Unix domain sockets in DogStatsD format.

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

### Custom Exporters

Implement the `Exporter` trait:

```rust
#[async_trait::async_trait]
impl Exporter for MyExporter {
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        for m in metrics {
            // m.metric_name, m.tags.pairs, m.value
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
