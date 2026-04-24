//! # chalk-metrics
//!
//! Efficient metrics aggregation with Rust macro definitions and pluggable
//! exporters.
//!
//! ## Overview
//!
//! Define your metrics with Rust macros, then record metrics with
//! zero-allocation hot-path performance. Each
//! metric is a struct with a `.record()` method that enforces the correct
//! value type at compile time.
//!
//! ## Quick Start
//!
//! **1. Define tags, namespaces, and metrics in Rust:**
//!
//! ```rust,ignore
//! chalk_metrics::define_tags! {
//!     pub Status => "status" {
//!         Success => "success",
//!         Failure => "failure",
//!     },
//!     pub Endpoint => "endpoint";
//! }
//!
//! chalk_metrics::define_namespaces! {
//!     pub Http => "http";
//! }
//!
//! chalk_metrics::define_metrics! {
//!     group(namespace = Http, tags = [Endpoint, Status]) {
//!         pub count HttpRequestCount => "request_count", "Total requests";
//!     }
//! }
//! ```
//!
//! **2. Initialize and record:**
//!
//! ```rust,ignore
//! use chalk_metrics::export::prometheus::PrometheusExporter;
//!
//! fn main() {
//!     chalk_metrics::client::builder()
//!         .with_exporter(PrometheusExporter::builder().build())
//!         .flush_interval(std::time::Duration::from_secs(10))
//!         .init();
//!
//!     HttpRequestCount {
//!         endpoint: Endpoint::from("/api"),
//!         status: Status::Success,
//!     }.record();  // count: no arg = increment by 1
//! }
//! ```

extern crate self as chalk_metrics;

pub use chalk_metrics_macros::{define_metrics, define_namespaces, define_tags};

#[doc(hidden)]
#[path = "private.rs"]
pub mod __private;
pub(crate) mod aggregator;
pub mod client;
pub mod export;
pub mod generated;
mod macros;
mod worker;
