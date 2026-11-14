//! Series are keyed by a 64-bit tag hash; the macros compare tag values on every
//! hit so a collision would be counted. With realistic distinct tag sets the
//! count must stay zero and every distinct set must be its own series.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalk_metrics::client;
use chalk_metrics::export::{ExportError, Exporter, FlushedMetric, FlushedValue};
use chalk_metrics::{define_metrics, define_tags};

define_tags! {
    Endpoint => "endpoint";
    Region => "region";
    Status => "status" {
        Success => "success",
        Failure => "failure",
    }
}

define_metrics! {
    group(tags = [Endpoint, Status, optional Region]) {
        count Requests => "requests", "Requests";
        gauge Depth => "depth", "Depth";
        histogram Latency => "latency", "Latency";
    }
}

/// (metric name, tag pairs, value) for every exported series.
type Series = Vec<(String, Vec<(String, String)>, f64)>;

#[derive(Default)]
struct Capture {
    series: Arc<Mutex<Series>>,
}

#[async_trait::async_trait]
impl Exporter for Capture {
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        let mut series = self.series.lock().unwrap();
        for metric in metrics {
            let value = match &metric.value {
                FlushedValue::Count(value) => *value as f64,
                FlushedValue::Gauge(value) => *value,
                FlushedValue::Histogram(sketch) => sketch.count() as f64,
            };
            let tags = metric
                .tags
                .pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            series.push((metric.metric_name.to_owned(), tags, value));
        }
        Ok(())
    }
}

#[test]
fn distinct_tag_sets_are_distinct_series_with_no_collisions() {
    assert_eq!(client::tag_hash_collisions(), 0, "zero before init");

    let capture = Capture::default();
    let series = Arc::clone(&capture.series);
    client::builder()
        .with_exporter(capture)
        .flush_interval(Duration::from_secs(3600))
        .init();

    // 2,500 endpoints x 2 statuses x (no region, region "eu") = 10,000 tag sets.
    let mut sets = 0;
    for round in 0..2 {
        sets = 0;
        for i in 0..2_500 {
            for status in [Status::Success, Status::Failure] {
                for region in [None, Some(Region::from("eu"))] {
                    let tags = || {
                        (
                            Endpoint::from(format!("/api/v1/items/{i}")),
                            status,
                            region.clone(),
                        )
                    };
                    let (endpoint, status, region) = tags();
                    Requests {
                        endpoint,
                        status,
                        region,
                    }
                    .record();
                    let (endpoint, status, region) = tags();
                    Latency {
                        endpoint,
                        status,
                        region,
                    }
                    .record(0.01);
                    if round == 0 {
                        let (endpoint, status, region) = tags();
                        Depth {
                            endpoint,
                            status,
                            region,
                        }
                        .record(i as f64);
                    }
                    sets += 1;
                }
            }
        }
    }
    assert_eq!(sets, 10_000);
    assert_eq!(client::tag_hash_collisions(), 0);

    client::shutdown();
    let series = series.lock().unwrap();
    for name in ["requests", "depth", "latency"] {
        let of_kind: Vec<_> = series.iter().filter(|(n, _, _)| n == name).collect();
        assert_eq!(of_kind.len(), 10_000, "{name}: one series per tag set");
        let distinct: std::collections::HashSet<_> =
            of_kind.iter().map(|(_, tags, _)| tags).collect();
        assert_eq!(distinct.len(), 10_000, "{name}: tag sets are all different");
    }
    // Equal tags recorded twice (two rounds) were aggregated into one series.
    for (name, tags, value) in series.iter() {
        match name.as_str() {
            "requests" | "latency" => assert_eq!(*value, 2.0, "{name} {tags:?}"),
            _ => {}
        }
    }
    assert_eq!(client::tag_hash_collisions(), 0);
}
