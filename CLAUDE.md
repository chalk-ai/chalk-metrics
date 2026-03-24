# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`chalk-metrics` is a Rust library crate (edition 2024) for efficient metrics aggregation with compile-time code generation and pluggable exporters.

## Commands

- **Build:** `cargo build`
- **Test all:** `cargo test`
- **Test single:** `cargo test <test_name>`
- **Lint:** `cargo clippy`
- **Format:** `cargo fmt`
- **Check:** `cargo check`
- **Run example:** `cargo run --example basic_usage`
- **Benchmark:** `cargo bench`

## Important Rules

- **Every change** must include a check: do docstrings, README.md, or crate-level docs in lib.rs need updating? If yes, update them in the same change. Documentation must always stay in sync with the code.

## Architecture

### Code Generation Pipeline
- Users define metrics in a JSON file with hierarchical namespaces (see `metrics.json`)
- `src/codegen.rs` exposes `generate(path)` (for external crates) and `generate_with_crate_path(path, crate_path)` (for internal use)
- Called from `build.rs` at compile time → generates `metrics_generated.rs` in `OUT_DIR`
- Each metric becomes a struct (e.g., `HttpRequestCount`) with a `.record()` method
- Count metrics: `.record()` (increment by 1) and `.record_value(i64)`
- Gauge/Histogram metrics: `.record(f64)`
- The struct IS the metric identity — no separate MetricId enum
- `src/schema.rs` defines the JSON schema serde types, namespace tree, and validation

### Recording API (Type-Safe)
- Each generated struct has `.record()` with the correct value type enforced at compile time
- Struct field names come from the tag's `export_name` (from JSON)
- Structs are namespace-prefixed for uniqueness: `HttpAuthLoginLatency` for `http > auth > login_latency`
- Associated consts `NAME` and `NAMESPACE` on each struct provide metric identity

### Aggregation
- `src/aggregator/striped_map.rs` — 64-stripe concurrent map using `parking_lot::Mutex` and `hashbrown::RawEntryMut`
- Uses `&'static str` pointer address for metric identity hashing (no u16 discriminant)
- `src/aggregator/count.rs` — lock-free `AtomicI64` (swap-to-zero on flush)
- `src/aggregator/gauge.rs` — lock-free `AtomicU64` storing f64 as bits (persists across flushes)
- `src/aggregator/sketch.rs` — UDD Sketch for approximate quantiles (from Timescale, Apache 2.0)
- `src/aggregator/histogram.rs` — `parking_lot::Mutex<UDDSketch>` (clone-and-reset on flush)

### Client & Worker
- `src/client.rs` — `OnceLock` singleton, builder pattern, `atexit` auto-shutdown
- `src/worker.rs` — dedicated OS thread with tokio runtime for periodic flush + export

### Exporters
- `src/export/mod.rs` — async `Exporter` trait; exporters receive `namespace: &[&str]` and decide how to format
- `src/export/prometheus.rs` — joins namespace segments with `_`
- `src/export/statsd.rs` — joins namespace segments with `.`; supports UDP/UDS with reconnection

### Tag Values
- Tag values use `Cow<'static, str>` — enum tags are `Cow::Borrowed(&'static str)` (zero allocation), string tags are `Cow::Owned(String)`
- `TagsData.pairs` is `Vec<(&'static str, Cow<'static, str>)>`

### Flush Behavior
- Counts and histograms are drained from the map on flush
- Gauges persist across flushes
- Tags data is `Arc`-shared — flush does a cheap pointer bump, no string copies
