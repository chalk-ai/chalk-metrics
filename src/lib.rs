//! # chalk-metrics
//!
//! Efficient metrics aggregation with compile-time code generation and
//! pluggable exporters.
//!
//! ## Overview
//!
//! Define your metrics in a JSON file, generate type-safe Rust code at build
//! time, then record metrics with zero-allocation hot-path performance. Each
//! metric is a struct with a `.record()` method that enforces the correct
//! value type at compile time.
//!
//! ## Quick Start
//!
//! **1. Define metrics in `metrics.json`:**
//!
//! ```json
//! {
//!     "tags": {
//!         "status": { "value_type": "enum", "values": ["success", "failure"], "export_name": "status" },
//!         "endpoint": { "value_type": "string", "export_name": "endpoint" }
//!     },
//!     "namespaces": {
//!         "http": {
//!             "metrics": [
//!                 { "name": "request_count", "type": "count", "tags": [{"tag": "endpoint"}, {"tag": "status"}], "description": "Total requests" }
//!             ]
//!         }
//!     }
//! }
//! ```
//!
//! **2. Add to your `build.rs`:**
//!
//! ```rust,ignore
//! fn main() {
//!     chalk_metrics::codegen::generate("metrics.json");
//! }
//! ```
//!
//! **3. Include generated code and use:**
//!
//! ```rust,ignore
//! mod metrics {
//!     include!(concat!(env!("OUT_DIR"), "/metrics_generated.rs"));
//! }
//!
//! use metrics::*;
//! use chalk_metrics::export::prometheus::PrometheusExporter;
//!
//! fn main() {
//!     chalk_metrics::client::builder()
//!         .with_exporter(PrometheusExporter::builder().build())
//!         .flush_interval(std::time::Duration::from_secs(10))
//!         .init();
//!
//!     // Type-safe recording — each struct IS the metric
//!     HttpRequestCount {
//!         endpoint: Endpoint::from("/api"),
//!         status: Status::Success,
//!     }.record();  // count: no arg = increment by 1
//! }
//! ```

pub(crate) mod schema;
pub mod codegen;
pub mod generated;
pub(crate) mod aggregator;
pub mod export;
pub mod client;
mod worker;
mod macros;
