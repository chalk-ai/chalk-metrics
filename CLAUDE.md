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

## Architecture

### Code Generation Pipeline
- Users define metrics in a JSON file (see `metrics.json` for the schema)
- `src/codegen.rs` exposes `generate(path)` — called from `build.rs` at compile time
- Generates `metrics_generated.rs` in `OUT_DIR`: tag types (enum/string), per-metric tag structs, `MetricId` enum
- `src/generated.rs` includes the generated code via `include!`
- `src/schema.rs` defines the JSON schema serde types and validation logic

### Aggregation
- `src/aggregator/striped_map.rs` — 64-stripe concurrent map using `parking_lot::Mutex` and `hashbrown::RawEntryMut` for zero-allocation hot-path
- `src/aggregator/count.rs` — lock-free `AtomicI64` counter (swap-to-zero on flush)
- `src/aggregator/gauge.rs` — lock-free `AtomicU64` gauge storing f64 as bits (persists across flushes)
- `src/aggregator/sketch.rs` — UDD Sketch for approximate quantile estimation (ported from Timescale, Apache 2.0)
- `src/aggregator/histogram.rs` — `parking_lot::Mutex<UDDSketch>` wrapper (clone-and-reset on flush)

### Client & Worker
- `src/client.rs` — `OnceLock` singleton, builder pattern (`init()`/`try_init()`), `atexit` auto-shutdown, recording free functions
- `src/worker.rs` — dedicated OS thread running tokio `current_thread` runtime for periodic flush + export
- `src/macros.rs` — `count!`, `gauge!`, `histogram!` macros connecting generated types to the recording API

### Exporters
- `src/export/mod.rs` — async `Exporter` trait, `FlushedMetric`/`FlushedValue` types
- `src/export/prometheus.rs` — Prometheus text format renderer with configurable bucket boundaries
- `src/export/statsd.rs` — DogStatsD format over UDP/UDS with batching, UDS reconnection, configurable histogram export mode

### Flush Behavior
- Counts and histograms are **drained** from the map on flush (re-created on next record)
- Gauges **persist** across flushes (last-value-wins semantics)
- Tags data is `Arc`-shared — flush does a cheap pointer bump, no string copies
