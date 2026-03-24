use std::sync::Arc;
use std::time::Duration;

use chalk_metrics::client;
use chalk_metrics::export::prometheus::PrometheusExporter;
use chalk_metrics::export::{ExportError, Exporter, FlushedMetric, FlushedValue};

use parking_lot::Mutex;

struct CollectingExporter {
    collected: Arc<Mutex<Vec<Vec<FlushedMetric>>>>,
}

#[async_trait::async_trait]
impl Exporter for CollectingExporter {
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        let batch: Vec<FlushedMetric> = metrics
            .iter()
            .map(|m| FlushedMetric {
                namespace: m.namespace,
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

    let local = client::builder()
        .flush_interval(Duration::from_secs(60))
        .build_local();

    local.record_count("request_count", &["http"], 0, || {
        vec![("endpoint", "/api".into()), ("status", "success".into())]
    }, 10);

    local.record_gauge("active_connections", &["http"], 1, || {
        vec![("endpoint", "/api".into())]
    }, 42.0);

    local.record_histogram("request_latency", &["http"], 2, || {
        vec![("endpoint", "/api".into()), ("status", "success".into())]
    }, 0.5);

    let flushed = local.flush();
    assert_eq!(flushed.len(), 3);

    let text = prom.render_metrics(&flushed);
    assert!(text.contains("test_http_request_count_total"), "text: {text}");
    assert!(text.contains("test_http_active_connections"), "text: {text}");
    assert!(text.contains("test_http_request_latency_bucket"), "text: {text}");

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

    local.record_count("test", &[], 0, || vec![], 1);

    std::thread::sleep(Duration::from_millis(150));
    assert!(!collected.lock().is_empty());
    local.shutdown();
}

#[test]
fn test_high_throughput() {
    let local = Arc::new(
        client::builder()
            .flush_interval(Duration::from_secs(60))
            .build_local(),
    );

    let handles: Vec<_> = (0..8)
        .map(|thread_id| {
            let local = Arc::clone(&local);
            std::thread::spawn(move || {
                for _ in 0..10_000 {
                    local.record_count(
                        "throughput_test",
                        &[],
                        thread_id,
                        || vec![("tid", std::borrow::Cow::Owned(format!("{thread_id}")))],
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

    for i in 1..=1000 {
        local.record_histogram("accuracy_test", &[], 0, || vec![], i as f64);
    }

    let flushed = local.flush();
    assert_eq!(flushed.len(), 1);

    match &flushed[0].value {
        FlushedValue::Histogram(sketch) => {
            assert_eq!(sketch.count(), 1000);
            let p50 = sketch.estimate_quantile(0.5);
            assert!((p50 - 500.0).abs() / 500.0 < 0.05, "p50 = {p50}");
            let p99 = sketch.estimate_quantile(0.99);
            assert!((p99 - 990.0).abs() / 990.0 < 0.05, "p99 = {p99}");
        }
        _ => panic!("expected histogram"),
    }

    local.shutdown();
}

#[test]
fn test_namespace_in_flushed_metrics() {
    let local = client::builder()
        .flush_interval(Duration::from_secs(60))
        .build_local();

    local.record_count("request_count", &["http"], 0, || vec![], 1);
    local.record_gauge("uptime", &[], 1, || vec![], 99.9);

    let flushed = local.flush();
    assert_eq!(flushed.len(), 2);

    let namespaced = flushed.iter().find(|m| m.metric_name == "request_count").unwrap();
    assert_eq!(namespaced.namespace, &["http"]);

    let top_level = flushed.iter().find(|m| m.metric_name == "uptime").unwrap();
    assert!(top_level.namespace.is_empty());

    local.shutdown();
}
