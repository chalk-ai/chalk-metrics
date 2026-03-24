use std::fmt::Write;
use std::sync::Arc;

use parking_lot::RwLock;

use super::{ExportError, Exporter, FlushedMetric, FlushedValue};

/// Default histogram bucket boundaries for Prometheus exposition format.
pub const DEFAULT_BUCKET_BOUNDARIES: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Prometheus text format exporter.
///
/// On each flush, renders all metrics into the Prometheus exposition text
/// format and stores the result for retrieval via [`get_metrics_text`].
///
/// This exporter does not serve HTTP — it is the caller's responsibility
/// to integrate the rendered text into an HTTP endpoint (e.g., via axum,
/// actix-web, or hyper).
///
/// # Example
///
/// ```rust,ignore
/// let prom = PrometheusExporter::builder()
///     .namespace("myapp")
///     .build();
/// // After metrics are flushed:
/// let text = prom.get_metrics_text();
/// // Serve `text` at /metrics
/// ```
pub struct PrometheusExporter {
    namespace: Option<String>,
    bucket_boundaries: Vec<f64>,
    latest: Arc<RwLock<String>>,
}

/// Builder for [`PrometheusExporter`].
pub struct PrometheusExporterBuilder {
    namespace: Option<String>,
    bucket_boundaries: Vec<f64>,
}

impl PrometheusExporterBuilder {
    /// Set a namespace prefix (prepended to all metric names with `_` separator).
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    /// Set custom histogram bucket boundaries. Defaults to
    /// `[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]`.
    pub fn bucket_boundaries(mut self, boundaries: Vec<f64>) -> Self {
        self.bucket_boundaries = boundaries;
        self
    }

    /// Build the exporter.
    pub fn build(self) -> PrometheusExporter {
        PrometheusExporter {
            namespace: self.namespace,
            bucket_boundaries: self.bucket_boundaries,
            latest: Arc::new(RwLock::new(String::new())),
        }
    }
}

impl PrometheusExporter {
    pub fn builder() -> PrometheusExporterBuilder {
        PrometheusExporterBuilder {
            namespace: None,
            bucket_boundaries: DEFAULT_BUCKET_BOUNDARIES.to_vec(),
        }
    }

    /// Get the latest rendered Prometheus text format output.
    pub fn get_metrics_text(&self) -> String {
        self.latest.read().clone()
    }

    /// Returns a clone of the internal Arc for sharing with HTTP handlers.
    pub fn text_handle(&self) -> Arc<RwLock<String>> {
        Arc::clone(&self.latest)
    }

    fn full_name(&self, namespace: &[&str], metric_name: &str) -> String {
        let mut parts = Vec::new();
        if let Some(ns) = &self.namespace {
            parts.push(ns.as_str());
        }
        parts.extend_from_slice(namespace);
        parts.push(metric_name);
        parts.join("_")
    }

    /// Render metrics into Prometheus text exposition format.
    pub fn render_metrics(&self, metrics: &[FlushedMetric]) -> String {
        self.render(metrics)
    }

    fn render(&self, metrics: &[FlushedMetric]) -> String {
        let mut out = String::with_capacity(4096);

        for metric in metrics {
            let name = self.full_name(metric.namespace, metric.metric_name);
            let tags_str = format_tags(&metric.tags.pairs);

            match &metric.value {
                FlushedValue::Count(value) => {
                    writeln!(out, "# HELP {name}_total {name}").unwrap();
                    writeln!(out, "# TYPE {name}_total counter").unwrap();
                    writeln!(out, "{name}_total{tags_str} {value}").unwrap();
                }
                FlushedValue::Gauge(value) => {
                    writeln!(out, "# HELP {name} {name}").unwrap();
                    writeln!(out, "# TYPE {name} gauge").unwrap();
                    writeln!(out, "{name}{tags_str} {value}").unwrap();
                }
                FlushedValue::Histogram(sketch) => {
                    writeln!(out, "# HELP {name} {name}").unwrap();
                    writeln!(out, "# TYPE {name} histogram").unwrap();

                    let count = sketch.count();
                    let sum = sketch.sum();

                    // Emit _bucket lines for each boundary
                    let mut cumulative = 0u64;
                    for &le in &self.bucket_boundaries {
                        // Estimate how many values are <= le
                        let quantile_at = sketch.estimate_quantile_at_value(le);
                        let bucket_count = (quantile_at * count as f64).round() as u64;
                        cumulative = cumulative.max(bucket_count);

                        let le_str = format_le(le);
                        if tags_str.is_empty() {
                            writeln!(out, "{name}_bucket{{le=\"{le_str}\"}} {cumulative}")
                                .unwrap();
                        } else {
                            // Insert le into existing tags
                            writeln!(
                                out,
                                "{name}_bucket{{{},le=\"{le_str}\"}} {cumulative}",
                                &tags_str[1..tags_str.len() - 1]
                            )
                            .unwrap();
                        }
                    }

                    // +Inf bucket
                    if tags_str.is_empty() {
                        writeln!(out, "{name}_bucket{{le=\"+Inf\"}} {count}").unwrap();
                    } else {
                        writeln!(
                            out,
                            "{name}_bucket{{{},le=\"+Inf\"}} {count}",
                            &tags_str[1..tags_str.len() - 1]
                        )
                        .unwrap();
                    }

                    writeln!(out, "{name}_sum{tags_str} {sum}").unwrap();
                    writeln!(out, "{name}_count{tags_str} {count}").unwrap();
                }
            }
        }

        out
    }
}

fn format_tags(pairs: &[(&str, std::borrow::Cow<'static, str>)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let inner: Vec<String> = pairs.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
    format!("{{{}}}", inner.join(","))
}

fn format_le(le: f64) -> String {
    // Use compact representation: avoid trailing zeros
    if le == le.floor() && le.abs() < 1e15 {
        format!("{}", le as i64)
    } else {
        format!("{le}")
    }
}

#[async_trait::async_trait]
impl Exporter for PrometheusExporter {
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        let text = self.render(metrics);
        *self.latest.write() = text;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::sketch::UDDSketch;
    use crate::aggregator::striped_map::TagsData;
    use std::sync::Arc;

    fn make_tags(pairs: Vec<(&'static str, std::borrow::Cow<'static, str>)>) -> Arc<TagsData> {
        Arc::new(TagsData { pairs })
    }

    #[test]
    fn test_counter_format() {
        let exp = PrometheusExporter::builder().build();
        let metrics = vec![FlushedMetric {
            namespace: &[],
            metric_name: "request_count",
            tags: make_tags(vec![]),
            value: FlushedValue::Count(42),
        }];
        let text = exp.render(&metrics);
        assert!(text.contains("# HELP request_count_total"));
        assert!(text.contains("# TYPE request_count_total counter"));
        assert!(text.contains("request_count_total 42"));
    }

    #[test]
    fn test_gauge_format() {
        let exp = PrometheusExporter::builder().build();
        let metrics = vec![FlushedMetric {
            namespace: &[],
            metric_name: "temperature",
            tags: make_tags(vec![]),
            value: FlushedValue::Gauge(23.5),
        }];
        let text = exp.render(&metrics);
        assert!(text.contains("# TYPE temperature gauge"));
        assert!(text.contains("temperature 23.5"));
    }

    #[test]
    fn test_histogram_format() {
        let exp = PrometheusExporter::builder()
            .bucket_boundaries(vec![1.0, 5.0, 10.0])
            .build();

        let mut sketch = UDDSketch::new(200, 0.001);
        for i in 1..=100 {
            sketch.add_value(i as f64 / 10.0);
        }

        let metrics = vec![FlushedMetric {
            namespace: &[],
            metric_name: "latency",
            tags: make_tags(vec![]),
            value: FlushedValue::Histogram(sketch),
        }];
        let text = exp.render(&metrics);
        assert!(text.contains("# TYPE latency histogram"));
        assert!(text.contains("latency_bucket{le=\"1\"}"));
        assert!(text.contains("latency_bucket{le=\"5\"}"));
        assert!(text.contains("latency_bucket{le=\"10\"}"));
        assert!(text.contains("latency_bucket{le=\"+Inf\"} 100"));
        assert!(text.contains("latency_sum"));
        assert!(text.contains("latency_count 100"));
    }

    #[test]
    fn test_tags_formatting() {
        let exp = PrometheusExporter::builder().build();
        let metrics = vec![FlushedMetric {
            namespace: &[],
            metric_name: "req",
            tags: make_tags(vec![
                ("endpoint", "/api".into()),
                ("status", "success".into()),
            ]),
            value: FlushedValue::Count(1),
        }];
        let text = exp.render(&metrics);
        assert!(text.contains("req_total{endpoint=\"/api\",status=\"success\"} 1"));
    }

    #[test]
    fn test_namespace_prefix() {
        let exp = PrometheusExporter::builder().namespace("myapp").build();
        let metrics = vec![FlushedMetric {
            namespace: &[],
            metric_name: "req",
            tags: make_tags(vec![]),
            value: FlushedValue::Gauge(1.0),
        }];
        let text = exp.render(&metrics);
        assert!(text.contains("myapp_req 1"));
    }

    #[test]
    fn test_histogram_with_tags() {
        let exp = PrometheusExporter::builder()
            .bucket_boundaries(vec![1.0])
            .build();

        let mut sketch = UDDSketch::new(200, 0.001);
        sketch.add_value(0.5);

        let metrics = vec![FlushedMetric {
            namespace: &[],
            metric_name: "lat",
            tags: make_tags(vec![("ep", "/api".into())]),
            value: FlushedValue::Histogram(sketch),
        }];
        let text = exp.render(&metrics);
        assert!(text.contains("lat_bucket{ep=\"/api\",le=\"1\"} 1"));
        assert!(text.contains("lat_bucket{ep=\"/api\",le=\"+Inf\"} 1"));
    }

    #[test]
    fn test_metric_namespace() {
        let exp = PrometheusExporter::builder().build();
        let metrics = vec![FlushedMetric {
            namespace: &["http"],
            metric_name: "request_count",
            tags: make_tags(vec![]),
            value: FlushedValue::Count(5),
        }];
        let text = exp.render(&metrics);
        assert!(text.contains("http_request_count_total 5"), "text: {text}");
    }

    #[test]
    fn test_nested_namespace_with_exporter_prefix() {
        let exp = PrometheusExporter::builder().namespace("myapp").build();
        let metrics = vec![FlushedMetric {
            namespace: &["http", "auth"],
            metric_name: "login_latency",
            tags: make_tags(vec![]),
            value: FlushedValue::Gauge(0.5),
        }];
        let text = exp.render(&metrics);
        assert!(text.contains("myapp_http_auth_login_latency 0.5"), "text: {text}");
    }
}
