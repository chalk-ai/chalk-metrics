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

/// A single raw histogram value recorded when `aggregate_distr_metrics(false)`
/// is set on the client, bypassing sketch aggregation for this data point.
///
/// `tags` is the same `Arc<TagsData>` cached by the striped aggregation map
/// for this metric's key, so producing one of these does not allocate beyond
/// the first-seen tag combination.
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

/// Trait for exporting aggregated metrics to an external system.
///
/// Implementors receive a slice of [`FlushedMetric`] on each flush interval
/// and are responsible for formatting and sending them to their destination
/// (e.g., StatsD, Prometheus, custom backends).
///
/// The trait is async to support network-bound exporters. The flush worker
/// calls all registered exporters concurrently.
#[async_trait::async_trait]
pub trait Exporter: Send + Sync {
    /// Export the given batch of flushed metrics.
    ///
    /// Called once per flush interval with all metrics that were aggregated
    /// since the last flush. Errors are logged but do not halt the flush loop.
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError>;

    /// Export raw, unaggregated distribution points recorded while
    /// `aggregate_distr_metrics(false)` is set on the client.
    ///
    /// Default is a no-op; only exporters that support per-event distribution
    /// fidelity (e.g. StatsD) need to override this.
    async fn export_raw_points(&self, _points: &[RawDistributionPoint]) -> Result<(), ExportError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ExportOnlyExporter;

    #[async_trait::async_trait]
    impl Exporter for ExportOnlyExporter {
        async fn export(&self, _metrics: &[FlushedMetric]) -> Result<(), ExportError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_default_export_raw_points_is_noop() {
        let exporter = ExportOnlyExporter;
        let points: Vec<RawDistributionPoint> = Vec::new();
        assert!(exporter.export_raw_points(&points).await.is_ok());
    }
}
