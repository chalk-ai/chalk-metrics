use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalk_metrics::__private::MetricTag;
use chalk_metrics::export::{
    DistributionInput, ExportError, Exporter, FlushedMetric, FlushedValue, RawDistributionPoint,
};
use chalk_metrics::{client, define_metrics};

#[derive(Debug, Clone, PartialEq, Eq)]
struct CollidingTag(String);

impl Hash for CollidingTag {
    fn hash<H: Hasher>(&self, _state: &mut H) {
        // Exercise generated comparisons with deterministic tag-hash collisions.
    }
}

impl MetricTag for CollidingTag {
    const EXPORT_NAME: &'static str = "original";

    fn export_value(&self) -> Cow<'static, str> {
        self.0.clone().into()
    }
}

define_metrics! {
    group(tags = [CollidingTag as tenant, optional CollidingTag as zone]) {
        count Count => "count", "Counter";
        gauge Gauge => "gauge", "Gauge";
        histogram Histogram => "histogram", "Histogram";
    }
    group(tags = [CollidingTag as other, optional CollidingTag as zone]) {
        count OtherKey => "count", "Different exported tag key";
    }
}

type Sample = (String, Vec<(&'static str, String)>, f64);

struct Capture {
    aggregates: Arc<Mutex<Vec<Sample>>>,
    raw: Arc<Mutex<Vec<Sample>>>,
}

fn sample(name: &str, pairs: &[(&'static str, Cow<'static, str>)], value: f64) -> Sample {
    (
        name.to_owned(),
        pairs
            .iter()
            .map(|(key, value)| (*key, value.to_string()))
            .collect(),
        value,
    )
}

#[async_trait::async_trait]
impl Exporter for Capture {
    fn distribution_input(&self) -> Option<DistributionInput> {
        Some(DistributionInput::Both)
    }

    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        let mut captured = self.aggregates.lock().unwrap();
        for metric in metrics {
            let value = match &metric.value {
                FlushedValue::Count(value) => *value as f64,
                FlushedValue::Gauge(value) => *value,
                FlushedValue::Histogram(sketch) => {
                    assert_eq!(sketch.count(), 2);
                    sketch.sum()
                }
            };
            captured.push(sample(metric.metric_name, &metric.tags.pairs, value));
        }
        Ok(())
    }

    async fn export_raw_points(&self, points: &[RawDistributionPoint]) -> Result<(), ExportError> {
        self.raw.lock().unwrap().extend(
            points
                .iter()
                .map(|point| sample(point.metric_name, &point.tags.pairs, point.value)),
        );
        Ok(())
    }
}

#[test]
fn generated_metrics_compare_optional_aliased_tags_on_hash_matches() {
    let aggregates = Arc::new(Mutex::new(Vec::new()));
    let raw = Arc::new(Mutex::new(Vec::new()));
    client::builder()
        .flush_interval(Duration::from_secs(3600))
        .with_exporter(Capture {
            aggregates: Arc::clone(&aggregates),
            raw: Arc::clone(&raw),
        })
        .init();
    let mut expected = Vec::new();
    let mut expected_raw = Vec::new();
    for series in 0..9 {
        let tenant = series.to_string();
        let zone = format!("zone-{}", series % 2);
        let pairs = vec![("tenant", tenant.clone()), ("zone", zone.clone())];
        for round in [1, 10] {
            let value = (series + 1) * round;
            Count {
                tenant: CollidingTag(tenant.clone()),
                zone: Some(CollidingTag(zone.clone())),
            }
            .record_value(value);
            Gauge {
                tenant: CollidingTag(tenant.clone()),
                zone: Some(CollidingTag(zone.clone())),
            }
            .record(value as f64);
            Histogram {
                tenant: CollidingTag(tenant.clone()),
                zone: Some(CollidingTag(zone.clone())),
            }
            .record(value as f64);
            expected_raw.push(("histogram".to_owned(), pairs.clone(), value as f64));
        }
        expected.push((
            "count".to_owned(),
            pairs.clone(),
            ((series + 1) * 11) as f64,
        ));
        expected.push((
            "gauge".to_owned(),
            pairs.clone(),
            ((series + 1) * 10) as f64,
        ));
        expected.push(("histogram".to_owned(), pairs, ((series + 1) * 11) as f64));
    }
    // Equal values and hashes with a different alias still identify a distinct series.
    OtherKey {
        other: CollidingTag("0".into()),
        zone: Some(CollidingTag("zone-0".into())),
    }
    .record_value(42);
    expected.push((
        "count".into(),
        vec![("other", "0".into()), ("zone", "zone-0".into())],
        42.0,
    ));
    Count {
        tenant: CollidingTag("0".into()),
        zone: None,
    }
    .record_value(7);
    expected.push(("count".into(), vec![("tenant", "0".into())], 7.0));
    client::shutdown();
    let mut captured = aggregates.lock().unwrap();
    captured.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    expected.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    assert_eq!(*captured, expected);
    assert_eq!(*raw.lock().unwrap(), expected_raw);
}
