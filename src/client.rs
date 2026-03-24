use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Notify;

use crate::aggregator::striped_map::StripedAggMap;
use crate::export::Exporter;
use crate::worker;

static GLOBAL: OnceLock<MetricsClient> = OnceLock::new();
static SHUTDOWN_CALLED: AtomicBool = AtomicBool::new(false);

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
    worker_threads: usize,
    max_buckets: u64,
    initial_error: f64,
}

impl MetricsClientBuilder {
    fn new() -> Self {
        Self {
            exporters: Vec::new(),
            flush_interval: Duration::from_secs(10),
            worker_threads: 1,
            max_buckets: 200,
            initial_error: 0.001,
        }
    }

    /// Register an exporter. Multiple exporters can be registered.
    pub fn with_exporter(mut self, exporter: impl Exporter + 'static) -> Self {
        self.exporters.push(Box::new(exporter));
        self
    }

    /// Set the flush interval (default: 10 seconds).
    pub fn flush_interval(mut self, interval: Duration) -> Self {
        self.flush_interval = interval;
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

    /// Set the initial histogram error bound (default: 0.001).
    pub fn initial_error(mut self, e: f64) -> Self {
        self.initial_error = e;
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
        GLOBAL
            .set(client)
            .map_err(|_| AlreadyInitializedError)?;

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
        let aggregator = Arc::new(StripedAggMap::new(self.max_buckets, self.initial_error));
        let shutdown_notify = Arc::new(Notify::new());

        let handle = worker::spawn_flush_worker(
            Arc::clone(&aggregator),
            self.exporters,
            self.flush_interval,
            self.worker_threads,
            Arc::clone(&shutdown_notify),
        );

        MetricsClient {
            aggregator,
            shutdown_notify,
            worker_handle: std::sync::Mutex::new(Some(handle)),
        }
    }
}

impl MetricsClient {
    /// Shut down the flush worker, performing a final flush.
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
    if let Some(client) = GLOBAL.get() {
        client
            .aggregator
            .record_count(metric_name, namespace, tags_hash, make_tags, delta);
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
    if let Some(client) = GLOBAL.get() {
        client
            .aggregator
            .record_gauge(metric_name, namespace, tags_hash, make_tags, value);
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
    if let Some(client) = GLOBAL.get() {
        client
            .aggregator
            .record_histogram(metric_name, namespace, tags_hash, make_tags, value);
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

    /// Manually trigger a flush and return the flushed metrics.
    pub fn flush(&self) -> Vec<crate::export::FlushedMetric> {
        self.aggregator.flush()
    }
}

#[cfg(test)]
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
        let client = builder().flush_interval(Duration::from_secs(60)).build_local();

        client.record_count("test_count", &[], 100, || vec![("k", "v".into())], 5);
        client.record_gauge("test_gauge", &[], 200, || vec![], 42.0);
        client.record_histogram("test_hist", &[], 300, || vec![], 10.0);

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

        client.record_count("c", &[], 100, || vec![], 1);

        std::thread::sleep(Duration::from_millis(150));
        assert!(export_count.load(Ordering::Relaxed) > 0);
        client.shutdown();
    }

    #[test]
    fn test_noop_when_not_initialized() {
        record_count("noop", &[], 0, || vec![], 1);
        record_gauge("noop", &[], 0, || vec![], 1.0);
        record_histogram("noop", &[], 0, || vec![], 1.0);
    }

    #[test]
    fn test_shutdown_is_idempotent() {
        let client = builder().flush_interval(Duration::from_secs(60)).build_local();
        client.shutdown();
        client.shutdown();
    }
}
