use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

use crate::__private::MetricName;
use crate::aggregator::striped_map::StripedAggMap;
use crate::export::{ConfiguredExporter, DistributionInput, Exporter};
use crate::worker;

static GLOBAL: OnceLock<MetricsClient> = OnceLock::new();
static SHUTDOWN_CALLED: AtomicBool = AtomicBool::new(false);

const RAW_POINT_CHANNEL_CAPACITY: usize = 16_384;

/// The metrics client. Holds the aggregation map and manages the background
/// flush worker thread.
pub struct MetricsClient {
    aggregator: Arc<StripedAggMap>,
    shutdown_notify: Arc<Notify>,
    worker_handle: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// Error returned when attempting to initialize the metrics client more than once.
#[derive(Debug)]
pub struct AlreadyInitializedError;

impl std::fmt::Display for AlreadyInitializedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "chalk-metrics client already initialized")
    }
}

impl std::error::Error for AlreadyInitializedError {}

/// Builder for configuring and initializing the metrics client.
///
/// # Example
///
/// ```rust,ignore
/// chalk_metrics::client::builder()
///     .with_exporter(PrometheusExporter::builder().build())
///     .flush_interval(Duration::from_secs(10))
///     .init();
/// ```
pub struct MetricsClientBuilder {
    exporters: Vec<Box<dyn Exporter>>,
    flush_interval: Duration,
    raw_sample_flush_interval: Duration,
    worker_threads: usize,
    max_buckets: u64,
    initial_error: f64,
    aggregate_distr_metrics: bool,
    raw_sample_queue_capacity: usize,
}

impl MetricsClientBuilder {
    fn new() -> Self {
        Self {
            exporters: Vec::new(),
            flush_interval: Duration::from_secs(10),
            raw_sample_flush_interval: Duration::from_millis(100),
            worker_threads: 1,
            max_buckets: 200,
            initial_error: 0.001,
            aggregate_distr_metrics: true,
            raw_sample_queue_capacity: RAW_POINT_CHANNEL_CAPACITY,
        }
    }

    /// Register an exporter. Multiple exporters can be registered.
    pub fn with_exporter(mut self, exporter: impl Exporter + 'static) -> Self {
        self.exporters.push(Box::new(exporter));
        self
    }

    /// Set the counter, gauge, and sketch flush interval (default: 10 seconds).
    /// Raw observations use `raw_sample_flush_interval` independently.
    pub fn flush_interval(mut self, interval: Duration) -> Self {
        assert!(!interval.is_zero(), "flush interval must be positive");
        self.flush_interval = interval;
        self
    }

    /// Set the raw-observation export interval (default: 100 milliseconds),
    /// independently of the counter, gauge, and sketch flush interval.
    /// Queued observations are exported one interval after the previous
    /// export, or sooner once the queue is a quarter full (checked every tenth
    /// of the interval, but no more often than once per millisecond).
    /// Sequential or slow exporters can extend this latency.
    ///
    /// # Panics
    /// Panics if the interval is zero.
    pub fn raw_sample_flush_interval(mut self, interval: Duration) -> Self {
        assert!(
            !interval.is_zero(),
            "raw sample flush interval must be positive"
        );
        self.raw_sample_flush_interval = interval;
        self
    }

    /// Set the number of tokio worker threads (default: 1).
    pub fn worker_threads(mut self, n: usize) -> Self {
        self.worker_threads = n.max(1);
        self
    }

    /// Set the maximum number of histogram buckets (default: 200).
    pub fn max_buckets(mut self, n: u64) -> Self {
        self.max_buckets = n;
        self
    }

    /// Set the relative error histograms start with (default: 0.001).
    ///
    /// This is not a permanent bound: when a histogram needs more than
    /// [`max_buckets`](Self::max_buckets) buckets, adjacent buckets are merged
    /// and the error grows. After compaction it depends on the bucket count and
    /// the value range, and is at most `tanh(ln(max / min) / max_buckets)`.
    pub fn initial_error(mut self, e: f64) -> Self {
        self.initial_error = e;
        self
    }

    /// Legacy fallback for custom exporters without an explicit distribution input.
    /// `false` also preserves Distribution output for StatsD with no explicit mode.
    /// Explicit StatsD modes and other built-in requirements take precedence.
    /// Migrate to an explicit exporter mode before removing this call.
    #[deprecated(
        note = "set an explicit StatsD histogram_mode before removing legacy false; custom exporters should implement distribution_input"
    )]
    pub fn aggregate_distr_metrics(mut self, enabled: bool) -> Self {
        self.aggregate_distr_metrics = enabled;
        self
    }

    /// Set the bounded raw-sample queue capacity (default: 16,384). A quarter
    /// of this capacity is the level at which queued samples are exported
    /// before the raw flush interval elapses.
    /// Recording never blocks: a full queue drops the newest raw observation,
    /// counted by `MetricsClient::dropped_raw_samples`. Sketches remain complete.
    ///
    /// # Panics
    /// Panics if capacity is zero.
    pub fn raw_sample_queue_capacity(mut self, capacity: usize) -> Self {
        assert!(capacity > 0, "raw sample queue capacity must be positive");
        self.raw_sample_queue_capacity = capacity;
        self
    }

    /// Initialize the global metrics client singleton.
    ///
    /// # Panics
    /// Panics if called more than once.
    pub fn init(self) {
        self.try_init()
            .expect("chalk-metrics already initialized; init() called twice");
    }

    /// Try to initialize the global metrics client singleton.
    pub fn try_init(self) -> Result<(), AlreadyInitializedError> {
        let client = self.build_inner();
        GLOBAL.set(client).map_err(|_| AlreadyInitializedError)?;

        unsafe {
            libc::atexit(atexit_handler);
        }

        Ok(())
    }

    /// Build a non-global client instance, useful for testing.
    pub fn build_local(self) -> MetricsClient {
        self.build_inner()
    }

    fn build_inner(self) -> MetricsClient {
        let fallback = if self.aggregate_distr_metrics {
            DistributionInput::Sketch
        } else {
            DistributionInput::Raw
        };
        let exporters: Vec<_> = self
            .exporters
            .into_iter()
            .map(|mut exporter| {
                let input = exporter
                    .configure_distribution(self.aggregate_distr_metrics)
                    .unwrap_or(fallback);
                ConfiguredExporter { exporter, input }
            })
            .collect();
        // No-exporter clients retain the local flush/legacy recording behavior.
        let wants_raw = exporters.iter().any(|e| e.input.wants_raw())
            || (exporters.is_empty() && fallback.wants_raw());
        let wants_sketch = exporters.iter().any(|e| e.input.wants_sketch())
            || (exporters.is_empty() && fallback.wants_sketch());
        let (raw_sender, raw_receiver) = if wants_raw {
            let (tx, rx) = tokio::sync::mpsc::channel(self.raw_sample_queue_capacity);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let aggregator = Arc::new(
            StripedAggMap::new(self.max_buckets, self.initial_error, raw_sender)
                .with_histogram_aggregation(wants_sketch),
        );
        let shutdown_notify = Arc::new(Notify::new());

        let handle = worker::spawn_flush_worker(
            Arc::clone(&aggregator),
            exporters,
            self.flush_interval,
            self.raw_sample_flush_interval,
            self.worker_threads,
            Arc::clone(&shutdown_notify),
            raw_receiver,
        );

        MetricsClient {
            aggregator,
            shutdown_notify,
            worker_handle: std::sync::Mutex::new(Some(handle)),
        }
    }
}

impl MetricsClient {
    /// Number of raw observations rejected by a full or closed queue. This does
    /// not count transport failures, which are logged by the worker.
    /// Histogram sketches are unaffected by raw queue overflow.
    pub fn dropped_raw_samples(&self) -> u64 {
        self.aggregator.dropped_raw_samples()
    }

    /// Shut down the flush worker, draining accepted raw samples and performing
    /// a final export before joining the worker. Stop all recording threads first.
    /// This blocks on exporters and must not be called from an exporter callback.
    pub fn shutdown(&self) {
        self.shutdown_notify.notify_one();
        if let Ok(mut guard) = self.worker_handle.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

/// Create a new builder for the metrics client.
pub fn builder() -> MetricsClientBuilder {
    MetricsClientBuilder::new()
}

/// Number of raw observations rejected by the global client's full or closed
/// queue. Returns zero before initialization. Transport failures are separate.
pub fn dropped_raw_samples() -> u64 {
    GLOBAL.get().map_or(0, MetricsClient::dropped_raw_samples)
}

/// Shut down the global metrics client. Safe to call multiple times.
pub fn shutdown() {
    if SHUTDOWN_CALLED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
        && let Some(client) = GLOBAL.get()
    {
        client.shutdown();
    }
}

extern "C" fn atexit_handler() {
    shutdown();
}

// ── Recording API ──────────────────────────────────────────────────────

/// Record a count delta. No-op if the client is not initialized.
#[inline]
pub fn record_count(
    metric_name: &'static str,
    namespace: &'static [&'static str],
    tags_hash: u64,
    make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
    delta: i64,
) {
    record_count_named(
        MetricName::new(metric_name),
        namespace,
        tags_hash,
        make_tags,
        delta,
    );
}

/// Recording support for macro-generated metrics with compile-time name fingerprints.
#[doc(hidden)]
#[inline]
pub fn record_count_named(
    name: MetricName,
    namespace: &'static [&'static str],
    tags_hash: u64,
    make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
    delta: i64,
) {
    if let Some(client) = GLOBAL.get() {
        client
            .aggregator
            .record_count_named(name, namespace, tags_hash, make_tags, delta);
    }
}

/// Record a gauge value. No-op if the client is not initialized.
#[inline]
pub fn record_gauge(
    metric_name: &'static str,
    namespace: &'static [&'static str],
    tags_hash: u64,
    make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
    value: f64,
) {
    record_gauge_named(
        MetricName::new(metric_name),
        namespace,
        tags_hash,
        make_tags,
        value,
    );
}

/// Recording support for macro-generated metrics with compile-time name fingerprints.
#[doc(hidden)]
#[inline]
pub fn record_gauge_named(
    name: MetricName,
    namespace: &'static [&'static str],
    tags_hash: u64,
    make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
    value: f64,
) {
    if let Some(client) = GLOBAL.get() {
        client
            .aggregator
            .record_gauge_named(name, namespace, tags_hash, make_tags, value);
    }
}

/// Record a histogram value. No-op if the client is not initialized.
#[inline]
pub fn record_histogram(
    metric_name: &'static str,
    namespace: &'static [&'static str],
    tags_hash: u64,
    make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
    value: f64,
) {
    record_histogram_named(
        MetricName::new(metric_name),
        namespace,
        tags_hash,
        make_tags,
        value,
    );
}

/// Recording support for macro-generated metrics with compile-time name fingerprints.
#[doc(hidden)]
#[inline]
pub fn record_histogram_named(
    name: MetricName,
    namespace: &'static [&'static str],
    tags_hash: u64,
    make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
    value: f64,
) {
    if let Some(client) = GLOBAL.get() {
        client
            .aggregator
            .record_histogram_named(name, namespace, tags_hash, make_tags, value);
    }
}

// ── Local client recording API (for testing) ───────────────────────────

impl MetricsClient {
    pub fn record_count(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        delta: i64,
    ) {
        self.aggregator
            .record_count(metric_name, namespace, tags_hash, make_tags, delta);
    }

    pub fn record_gauge(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        value: f64,
    ) {
        self.aggregator
            .record_gauge(metric_name, namespace, tags_hash, make_tags, value);
    }

    pub fn record_histogram(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        value: f64,
    ) {
        self.aggregator
            .record_histogram(metric_name, namespace, tags_hash, make_tags, value);
    }

    /// Drain aggregates for inspection, consuming data otherwise exported by the
    /// worker. This does not export metrics or drain the raw-sample queue.
    pub fn flush(&self) -> Vec<crate::export::FlushedMetric> {
        self.aggregator.flush()
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;
    use crate::export::{ExportError, FlushedMetric};
    use std::sync::atomic::AtomicUsize;

    struct CountingExporter {
        count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Exporter for CountingExporter {
        async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
            self.count.fetch_add(metrics.len(), Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn test_builder_defaults() {
        let b = MetricsClientBuilder::new();
        assert_eq!(b.flush_interval, Duration::from_secs(10));
        assert_eq!(b.worker_threads, 1);
    }

    #[test]
    fn test_local_client_record_and_flush() {
        let client = builder()
            .flush_interval(Duration::from_secs(60))
            .build_local();

        client.record_count("test_count", &[], 100, || vec![("k", "v".into())], 5);
        client.record_gauge("test_gauge", &[], 200, Vec::new, 42.0);
        client.record_histogram("test_hist", &[], 300, Vec::new, 10.0);

        let flushed = client.flush();
        assert_eq!(flushed.len(), 3);
        client.shutdown();
    }

    #[test]
    fn test_local_client_with_exporter() {
        let export_count = Arc::new(AtomicUsize::new(0));
        let exporter = CountingExporter {
            count: Arc::clone(&export_count),
        };

        let client = builder()
            .with_exporter(exporter)
            .flush_interval(Duration::from_millis(50))
            .build_local();

        client.record_count("c", &[], 100, Vec::new, 1);

        std::thread::sleep(Duration::from_millis(150));
        assert!(export_count.load(Ordering::Relaxed) > 0);
        client.shutdown();
    }

    struct RawPointCountingExporter {
        raw_count: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Exporter for RawPointCountingExporter {
        async fn export(&self, _metrics: &[FlushedMetric]) -> Result<(), ExportError> {
            Ok(())
        }

        async fn export_raw_points(
            &self,
            points: &[crate::export::RawDistributionPoint],
        ) -> Result<(), ExportError> {
            self.raw_count.fetch_add(points.len(), Ordering::Relaxed);
            Ok(())
        }
    }

    #[test]
    fn test_aggregate_distr_metrics_false_wires_raw_channel() {
        let raw_count = Arc::new(AtomicUsize::new(0));
        let exporter = RawPointCountingExporter {
            raw_count: Arc::clone(&raw_count),
        };

        let client = builder()
            .with_exporter(exporter)
            .flush_interval(Duration::from_millis(50))
            .aggregate_distr_metrics(false)
            .build_local();

        client.record_histogram("h", &[], 100, Vec::new, 1.0);

        std::thread::sleep(Duration::from_millis(150));
        assert!(raw_count.load(Ordering::Relaxed) > 0);
        client.shutdown();
    }

    #[test]
    fn test_aggregate_distr_metrics_default_does_not_wire_raw_channel() {
        let raw_count = Arc::new(AtomicUsize::new(0));
        let exporter = RawPointCountingExporter {
            raw_count: Arc::clone(&raw_count),
        };

        let client = builder()
            .with_exporter(exporter)
            .flush_interval(Duration::from_millis(50))
            .build_local();

        client.record_histogram("h", &[], 100, Vec::new, 1.0);

        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(raw_count.load(Ordering::Relaxed), 0);
        client.shutdown();
    }

    #[test]
    fn test_noop_when_not_initialized() {
        record_count("noop", &[], 0, Vec::new, 1);
        record_gauge("noop", &[], 0, Vec::new, 1.0);
        record_histogram("noop", &[], 0, Vec::new, 1.0);
        assert_eq!(dropped_raw_samples(), 0);
    }

    #[test]
    fn test_shutdown_is_idempotent() {
        let client = builder()
            .flush_interval(Duration::from_secs(60))
            .build_local();
        client.shutdown();
        client.shutdown();
    }
}
