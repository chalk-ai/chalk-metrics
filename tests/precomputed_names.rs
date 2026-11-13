use std::hash::Hasher;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalk_metrics::export::{ExportError, Exporter, FlushedMetric, FlushedValue};
use chalk_metrics::{client, define_metrics};

define_metrics! {
    group(tags = []) {
    count Count => "count", "Counter";
    gauge Gauge => "gauge", "Gauge";
    histogram Histogram => "histogram", "Histogram";
    }
}

struct Capture(Arc<Mutex<Vec<FlushedMetric>>>);
#[async_trait::async_trait]
impl Exporter for Capture {
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        self.0
            .lock()
            .unwrap()
            .extend(metrics.iter().map(|m| FlushedMetric {
                namespace: m.namespace,
                metric_name: m.metric_name,
                tags: Arc::clone(&m.tags),
                value: match &m.value {
                    FlushedValue::Count(v) => FlushedValue::Count(*v),
                    FlushedValue::Gauge(v) => FlushedValue::Gauge(*v),
                    FlushedValue::Histogram(v) => FlushedValue::Histogram(v.clone()),
                },
            }));
        Ok(())
    }
}

#[test]
fn generated_and_existing_recording_apis_share_series() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    client::builder()
        .with_exporter(Capture(Arc::clone(&captured)))
        .flush_interval(Duration::from_secs(3600))
        .init();
    Count {}.record();
    Gauge {}.record(1.0);
    Histogram {}.record(1.0);
    // Empty generated structs hash no fields. The existing string API must use
    // the same series as the compile-time descriptor emitted by the macro.
    let tags_hash = std::collections::hash_map::DefaultHasher::new().finish();
    let cached = || panic!("generated and string paths must share cached tags");
    client::record_count(Count::NAME, &[], tags_hash, cached, 99);
    client::record_gauge(Gauge::NAME, &[], tags_hash, cached, 99.0);
    client::record_histogram(Histogram::NAME, &[], tags_hash, cached, 99.0);
    client::shutdown();
    let metrics = captured.lock().unwrap();
    assert_eq!(metrics.len(), 3);
    for metric in metrics.iter() {
        match &metric.value {
            FlushedValue::Count(v) => assert_eq!(*v, 100),
            FlushedValue::Gauge(v) => assert_eq!(*v, 99.0),
            FlushedValue::Histogram(v) => {
                assert_eq!(v.count(), 2);
                assert_eq!(v.sum(), 100.0);
            }
        }
    }
}
