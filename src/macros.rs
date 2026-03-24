// Macros are no longer the primary recording API.
// Each generated metric struct has a .record() method with the correct value type.
// This module is kept for the optional convenience macro below.

/// Convenience macro for recording a metric value.
///
/// This is equivalent to calling `.record(value)` or `.record_value(value)`
/// on the metric struct directly.
///
/// # Example
///
/// ```rust,ignore
/// use chalk_metrics::generated::*;
///
/// // Count (defaults to increment by 1):
/// chalk_metrics::record!(HttpRequestCount { endpoint: Endpoint::from("/api"), status: Status::Success });
///
/// // Count with explicit delta:
/// chalk_metrics::record!(5, HttpRequestCount { endpoint: Endpoint::from("/api"), status: Status::Success });
///
/// // Gauge/Histogram (requires value):
/// chalk_metrics::record!(0.042, HttpRequestLatency { endpoint: Endpoint::from("/api"), status: Status::Success });
/// ```
#[macro_export]
macro_rules! record {
    // Count metric: no value means increment by 1
    ($tags:expr) => {
        $tags.record()
    };
    // Explicit value
    ($value:expr, $tags:expr) => {
        $tags.record($value)
    };
}
