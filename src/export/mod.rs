pub mod prometheus;
pub mod statsd;

use std::sync::Arc;

// Re-export types that appear in the public Exporter API.
// These originate in the aggregator (which is pub(crate)) but are part
// of the contract that custom Exporter implementations interact with.
pub use crate::aggregator::sketch::UDDSketch;
pub use crate::aggregator::striped_map::TagsData;

/// A flushed metric value ready for export.
#[derive(Debug)]
pub enum FlushedValue {
    Count(i64),
    Gauge(f64),
    Histogram(UDDSketch),
}

/// A single flushed metric with its identity, tags, and aggregated value.
///
/// Tag values in `tags.pairs` use `Cow<'static, str>`: enum-constrained tags
/// are `Cow::Borrowed` (zero allocation), while free-form string tags are
/// `Cow::Owned`.
#[derive(Debug)]
pub struct FlushedMetric {
    /// Namespace path segments (e.g., `["http", "auth"]`). Empty for top-level metrics.
    pub namespace: &'static [&'static str],
    pub metric_name: &'static str,
    pub tags: Arc<TagsData>,
    pub value: FlushedValue,
}

/// A single original histogram observation, delivered to raw-sample exporters.
/// Tags are shared with the aggregation entry for this series and flush period.
#[derive(Debug)]
pub struct RawDistributionPoint {
    pub namespace: &'static [&'static str],
    pub metric_name: &'static str,
    pub tags: Arc<TagsData>,
    pub value: f64,
}

/// Error returned by exporters.
#[derive(Debug)]
pub struct ExportError {
    pub message: String,
    pub source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)?;
        if let Some(ref src) = self.source {
            write!(f, ": {src}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ExportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|e| e.as_ref() as &(dyn std::error::Error + 'static))
    }
}

impl ExportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(
        message: impl Into<String>,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

/// Histogram representation required by an exporter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistributionInput {
    /// A histogram sketch containing every recorded observation.
    Sketch,
    /// Individual observations, subject to the bounded raw-sample queue.
    Raw,
    /// Both representations, delivered through their respective export methods
    /// on independent raw-sample and aggregate flush intervals.
    Both,
}

impl DistributionInput {
    pub(crate) fn wants_raw(self) -> bool {
        matches!(self, Self::Raw | Self::Both)
    }
    pub(crate) fn wants_sketch(self) -> bool {
        matches!(self, Self::Sketch | Self::Both)
    }
}

pub(crate) struct ConfiguredExporter {
    pub exporter: Box<dyn Exporter>,
    pub input: DistributionInput,
}

/// Trait for exporting aggregated metrics to an external system.
///
/// Implementors receive a slice of [`FlushedMetric`] on each aggregate flush interval
/// and are responsible for formatting and sending them to their destination
/// (e.g., StatsD, Prometheus, custom backends).
///
/// The trait is async to support network-bound exporters. The flush worker
/// calls registered exporters sequentially; increasing runtime threads does not
/// change this ordering.
///
/// Exporters run on the worker's tokio runtime. chalk-metrics does not enable
/// tokio's `net` feature; an exporter that uses `tokio::net` must enable it in
/// its own crate, which also enables the runtime's I/O driver.
#[async_trait::async_trait]
#[allow(
    clippy::double_must_use,
    reason = "async_trait annotates its generated boxed futures"
)]
pub trait Exporter: Send + Sync {
    /// Declare histogram requirements. Built-in exporters select this automatically.
    ///
    /// `None` preserves custom exporter compatibility: sketches by default, or raw
    /// samples with the deprecated `aggregate_distr_metrics(false)` setting.
    fn distribution_input(&self) -> Option<DistributionInput> {
        None
    }

    /// Client initialization hook. The default preserves custom exporters' requirements.
    /// StatsD uses legacy `false` as Distribution only when its mode was not set explicitly.
    #[doc(hidden)]
    fn configure_distribution(&mut self, _legacy_aggregate: bool) -> Option<DistributionInput> {
        self.distribution_input()
    }

    /// Export the given batch of flushed metrics.
    ///
    /// Called once per flush interval with all metrics that were aggregated
    /// since the last flush. Errors are logged but do not halt the flush loop.
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError>;

    /// Export original observations when this exporter requests raw samples.
    /// Called once per raw-sample flush interval (default: 100 milliseconds)
    /// since the previous call, or sooner once the raw queue is a quarter full,
    /// so batch sizes and call frequency vary with load.
    ///
    /// Default is a no-op; only exporters that support per-event distribution
    /// fidelity (e.g. StatsD) need to override this.
    async fn export_raw_points(&self, _points: &[RawDistributionPoint]) -> Result<(), ExportError> {
        Ok(())
    }
}
