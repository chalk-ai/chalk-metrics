use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalk_metrics::client;
use chalk_metrics::export::{
    DistributionInput, ExportError, Exporter, FlushedMetric, FlushedValue, RawDistributionPoint,
};

#[derive(Default)]
struct CaptureBoth {
    raw: Arc<Mutex<Vec<f64>>>,
    sketches: Arc<Mutex<Vec<(u64, f64)>>>,
}

#[async_trait::async_trait]
impl Exporter for CaptureBoth {
    fn distribution_input(&self) -> Option<DistributionInput> {
        Some(DistributionInput::Both)
    }
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        for metric in metrics {
            if let FlushedValue::Histogram(sketch) = &metric.value {
                self.sketches
                    .lock()
                    .unwrap()
                    .push((sketch.count(), sketch.sum()));
            }
        }
        Ok(())
    }
    async fn export_raw_points(&self, points: &[RawDistributionPoint]) -> Result<(), ExportError> {
        self.raw
            .lock()
            .unwrap()
            .extend(points.iter().map(|point| point.value));
        Ok(())
    }
}

#[test]
#[allow(deprecated)]
fn explicit_custom_requirement_overrides_either_legacy_toggle() {
    for legacy in [true, false] {
        let exporter = CaptureBoth::default();
        let raw = Arc::clone(&exporter.raw);
        let sketches = Arc::clone(&exporter.sketches);
        let client = client::builder()
            .with_exporter(exporter)
            .aggregate_distr_metrics(legacy)
            .flush_interval(Duration::from_secs(3600))
            .build_local();
        for value in [1.0, 99.0] {
            client.record_histogram("latency", &[], 42, Vec::new, value);
        }
        client.shutdown();
        assert_eq!(*raw.lock().unwrap(), [1.0, 99.0]);
        assert_eq!(*sketches.lock().unwrap(), [(2, 100.0)]);
    }
}
