# chalk-metrics

Efficient metrics aggregation with compile-time code generation and pluggable exporters.

Define your metrics in a JSON file, generate type-safe Rust code at build time, then record metrics with zero-allocation hot-path performance. Each metric is a struct — call `.record()` and the value type is enforced at compile time.

## Features

- **Type-safe recording**: Each metric is a struct with `.record(value)`. Count metrics accept `i64` (or no arg for +1), gauge/histogram accept `f64`. Wrong type = compile error.
- **Hierarchical namespaces**: Nest metrics under namespace blocks in JSON. Exporters receive namespace segments and format them (Prometheus uses `_`, StatsD uses `.`).
- **Zero-allocation hot path**: Striped `parking_lot` locks with `hashbrown::RawEntryMut` — only allocates on first-seen tag combinations.
- **Pluggable exporters**: Prometheus text format and StatsD/DogStatsD included, plus an async `Exporter` trait for custom backends.
- **Singleton client**: `env_logger`-style `init()`/`try_init()` with automatic shutdown via `atexit`.

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
            "name": "uptime",
            "type": "gauge",
            "tags": [],
            "description": "Process uptime in seconds"
        }
    ],
    "namespaces": {
        "http": {
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
    }
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
use metrics::*;
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
}
```

## JSON Schema Reference

### Tag Definitions

Tags are defined globally and reused across metrics.

| Field | Type | Description |
|---|---|---|
| `value_type` | `"enum"` or `"string"` | Whether values are constrained to a fixed set |
| `values` | `string[]` | Required for enum tags: the allowed values |
| `export_name` | `string` | Default key name used when exporting. Also used as the struct field name. |

### Namespaces

Metrics can be organized in hierarchical namespace blocks. Namespaces can be nested arbitrarily deep.

```json
{
    "namespaces": {
        "http": {
            "metrics": [...],
            "namespaces": {
                "auth": {
                    "metrics": [...]
                }
            }
        }
    },
    "metrics": [...]
}
```

- Metrics under `http` get namespace `["http"]`
- Metrics under `http > auth` get namespace `["http", "auth"]`
- Top-level metrics get namespace `[]`
- Struct names include the namespace path: `HttpAuthLoginLatency`

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
| `export_name` | `string?` | Override the export key and struct field name |
| `optional` | `bool` | Whether this tag can be omitted (default: false) |

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
