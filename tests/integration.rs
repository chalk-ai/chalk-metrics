use std::sync::Arc;
use std::time::Duration;

use chalk_metrics::client;
use chalk_metrics::export::prometheus::PrometheusExporter;
use chalk_metrics::export::{ExportError, Exporter, FlushedMetric, FlushedValue};
use chalk_metrics::generated::*;

use parking_lot::Mutex;

/// Collects all flushed metrics for inspection.
struct CollectingExporter {
    collected: Arc<Mutex<Vec<Vec<FlushedMetric>>>>,
}

#[async_trait::async_trait]
impl Exporter for CollectingExporter {
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        // We need to clone the metrics since they contain Arcs
        let batch: Vec<FlushedMetric> = metrics
            .iter()
            .map(|m| FlushedMetric {
                metric_name: m.metric_name,
                tags: Arc::clone(&m.tags),
                value: match &m.value {
                    FlushedValue::Count(v) => FlushedValue::Count(*v),
                    FlushedValue::Gauge(v) => FlushedValue::Gauge(*v),
                    FlushedValue::Histogram(s) => FlushedValue::Histogram(s.clone()),
                },
            })
            .collect();
        self.collected.lock().push(batch);
        Ok(())
    }
}

#[test]
fn test_full_pipeline_with_prometheus() {
    let prom = PrometheusExporter::builder().namespace("test").build();

    // Use a long flush interval so we control the flush manually
    let local = client::builder()
        .flush_interval(Duration::from_secs(60))
        .build_local();

    // Record metrics
    local.record_count(
        MetricId::RequestCount as u16,
        "request_count",
        0,
        || vec![("endpoint", "/api".into()), ("status", "success".into())],
        10,
    );

    local.record_gauge(
        MetricId::ActiveConnections as u16,
        "active_connections",
        1,
        || vec![("endpoint", "/api".into())],
        42.0,
    );

    local.record_histogram(
        MetricId::RequestLatency as u16,
        "request_latency",
        2,
        || vec![("endpoint", "/api".into()), ("status", "success".into())],
        0.5,
    );

    // Manual flush + render through Prometheus exporter
    let flushed = local.flush();
    assert_eq!(flushed.len(), 3, "expected 3 metrics, got {}", flushed.len());

    let text = prom.render_metrics(&flushed);
    assert!(text.contains("test_request_count_total"), "text: {text}");
    assert!(text.contains("test_active_connections"), "text: {text}");
    assert!(text.contains("test_request_latency_bucket"), "text: {text}");
    assert!(text.contains("test_request_latency_count"), "text: {text}");

    local.shutdown();
}

#[test]
fn test_multiple_exporters() {
    let collected = Arc::new(Mutex::new(Vec::new()));
    let collector = CollectingExporter {
        collected: Arc::clone(&collected),
    };
    let prom = PrometheusExporter::builder().build();

    let local = client::builder()
        .with_exporter(collector)
        .with_exporter(prom)
        .flush_interval(Duration::from_millis(50))
        .build_local();

    local.record_count(0, "test", 0, || vec![], 1);

    std::thread::sleep(Duration::from_millis(150));

    // Collecting exporter should have received at least one batch
    assert!(
        !collected.lock().is_empty(),
        "collecting exporter should have been called"
    );

    local.shutdown();
}

#[test]
fn test_high_throughput() {
    let local = Arc::new(
        client::builder()
            .flush_interval(Duration::from_secs(60)) // don't flush during test
            .build_local(),
    );

    let handles: Vec<_> = (0..8)
        .map(|thread_id| {
            let local = Arc::clone(&local);
            std::thread::spawn(move || {
                for _ in 0..10_000 {
                    local.record_count(
                        0,
                        "throughput_test",
                        thread_id, // different hash per thread = different stripe
                        || vec![("tid", format!("{thread_id}"))],
                        1,
                    );
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let flushed = local.flush();
    let total: i64 = flushed
        .iter()
        .filter_map(|m| match &m.value {
            FlushedValue::Count(v) => Some(*v),
            _ => None,
        })
        .sum();

    assert_eq!(total, 80_000);

    local.shutdown();
}

#[test]
fn test_histogram_quantile_accuracy() {
    let local = client::builder()
        .flush_interval(Duration::from_secs(60))
        .build_local();

    // Record a uniform distribution from 1 to 1000
    for i in 1..=1000 {
        local.record_histogram(0, "accuracy_test", 0, || vec![], i as f64);
    }

    let flushed = local.flush();
    assert_eq!(flushed.len(), 1);

    match &flushed[0].value {
        FlushedValue::Histogram(sketch) => {
            assert_eq!(sketch.count(), 1000);

            let p50 = sketch.estimate_quantile(0.5);
            assert!(
                (p50 - 500.0).abs() / 500.0 < 0.05,
                "p50 = {p50}, expected ~500"
            );

            let p99 = sketch.estimate_quantile(0.99);
            assert!(
                (p99 - 990.0).abs() / 990.0 < 0.05,
                "p99 = {p99}, expected ~990"
            );
        }
        _ => panic!("expected histogram"),
    }

    local.shutdown();
}
