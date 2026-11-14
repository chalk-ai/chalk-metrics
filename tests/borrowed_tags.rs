//! The borrowed `<Metric>Ref` structs record into the same series as the owned
//! structs: equal values aggregate together, different values do not, and no
//! hash collision is ever counted (an owned and a borrowed struct with equal
//! values must hash identically, or the series would split and a lookup would
//! fail to match).
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chalk_metrics::client;
use chalk_metrics::export::{ExportError, Exporter, FlushedMetric, FlushedValue};
use chalk_metrics::{define_metrics, define_namespaces, define_tags};

define_tags! {
    Endpoint => "endpoint";
    Region => "region";
    Status => "status" {
        Success => "success",
        Failure => "failure",
    }
}

define_namespaces! {
    Api => "api";
}

define_metrics! {
    group(tags = [Endpoint, Status]) {
        count Requests => "requests", "Requests";
        gauge Depth => "depth", "Depth";
        histogram Latency => "latency", "Latency";
    }
    group(tags = [Status]) {
        count EnumOnly => "enum_only", "Enum tag only";
    }
    group(namespace = Api, tags = [Endpoint, optional Region]) {
        count Optional => "optional", "Optional tag";
    }
    group(namespace = Api, tags = [Endpoint]) {
        count Aliased => "aliased", "Aliased tags",
            tags += [
                Status as outcome,
                optional Endpoint as upstream,
            ];
    }
}

/// (metric name, tag pairs, value) for every exported series.
type Series = Vec<(String, Vec<(String, String)>, f64)>;

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

fn pairs(tags: &[(&str, &str)]) -> Vec<(String, String)> {
    tags.iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

#[test]
fn borrowed_and_owned_records_share_series() {
    let series = Arc::new(Mutex::new(Vec::new()));
    client::builder()
        .with_exporter(Capture {
            series: Arc::clone(&series),
        })
        .flush_interval(Duration::from_secs(3600))
        .init();

    // String + enum, count: owned, borrowed, and `borrowed()` of the owned form.
    let owned = Requests {
        endpoint: Endpoint::from("/a"),
        status: Status::Success,
    };
    owned.record();
    RequestsRef {
        endpoint: "/a",
        status: Status::Success,
    }
    .record();
    owned.borrowed().record_value(3);
    // Different string value, different enum value: two more series.
    RequestsRef {
        endpoint: "/b",
        status: Status::Success,
    }
    .record();
    RequestsRef {
        endpoint: "/a",
        status: Status::Failure,
    }
    .record();

    // Gauge and histogram share series between the forms too.
    Depth {
        endpoint: Endpoint::from("/a"),
        status: Status::Success,
    }
    .record(1.0);
    DepthRef {
        endpoint: "/a",
        status: Status::Success,
    }
    .record(7.0);
    Latency {
        endpoint: Endpoint::from("/a"),
        status: Status::Success,
    }
    .record(0.01);
    LatencyRef {
        endpoint: "/a",
        status: Status::Success,
    }
    .record(0.02);

    // Enum-only metric: the `Ref` struct still takes a lifetime.
    EnumOnly {
        status: Status::Failure,
    }
    .record();
    EnumOnlyRef {
        status: Status::Failure,
    }
    .record();

    // Optional tag: `Some` and `None` are separate series, each shared.
    Optional {
        endpoint: Endpoint::from("/a"),
        region: Some(Region::from("eu")),
    }
    .record();
    OptionalRef {
        endpoint: "/a",
        region: Some("eu"),
    }
    .record();
    Optional {
        endpoint: Endpoint::from("/a"),
        region: None,
    }
    .record();
    OptionalRef {
        endpoint: "/a",
        region: None,
    }
    .record();
    OptionalRef {
        endpoint: "/a",
        region: Some("us"),
    }
    .record();

    // Aliased tags: the alias is the field name and the export key.
    Aliased {
        endpoint: Endpoint::from("/a"),
        outcome: Status::Success,
        upstream: Some(Endpoint::from("/u")),
    }
    .record();
    AliasedRef {
        endpoint: "/a",
        outcome: Status::Success,
        upstream: Some("/u"),
    }
    .record();
    AliasedRef {
        endpoint: "/a",
        outcome: Status::Success,
        upstream: None,
    }
    .record();

    assert_eq!(client::tag_hash_collisions(), 0);
    client::shutdown();

    let series = series.lock().unwrap();
    let find = |name: &str, tags: &[(&str, &str)]| -> Vec<f64> {
        series
            .iter()
            .filter(|(n, t, _)| n == name && *t == pairs(tags))
            .map(|(_, _, value)| *value)
            .collect()
    };
    let count_of = |name: &str| series.iter().filter(|(n, _, _)| n == name).count();

    assert_eq!(
        find("requests", &[("endpoint", "/a"), ("status", "success")]),
        [5.0]
    );
    assert_eq!(
        find("requests", &[("endpoint", "/b"), ("status", "success")]),
        [1.0]
    );
    assert_eq!(
        find("requests", &[("endpoint", "/a"), ("status", "failure")]),
        [1.0]
    );
    assert_eq!(count_of("requests"), 3);

    assert_eq!(
        find("depth", &[("endpoint", "/a"), ("status", "success")]),
        [7.0]
    );
    assert_eq!(count_of("depth"), 1);
    assert_eq!(
        find("latency", &[("endpoint", "/a"), ("status", "success")]),
        [2.0]
    );
    assert_eq!(count_of("latency"), 1);

    assert_eq!(find("enum_only", &[("status", "failure")]), [2.0]);
    assert_eq!(count_of("enum_only"), 1);

    assert_eq!(
        find("optional", &[("endpoint", "/a"), ("region", "eu")]),
        [2.0]
    );
    assert_eq!(find("optional", &[("endpoint", "/a")]), [2.0]);
    assert_eq!(
        find("optional", &[("endpoint", "/a"), ("region", "us")]),
        [1.0]
    );
    assert_eq!(count_of("optional"), 3);

    assert_eq!(
        find(
            "aliased",
            &[
                ("endpoint", "/a"),
                ("outcome", "success"),
                ("upstream", "/u")
            ]
        ),
        [2.0]
    );
    assert_eq!(
        find("aliased", &[("endpoint", "/a"), ("outcome", "success")]),
        [1.0]
    );
    assert_eq!(count_of("aliased"), 2);
}

#[test]
fn borrowed_round_trips() {
    let owned = Requests {
        endpoint: Endpoint::from("/a"),
        status: Status::Success,
    };
    let borrowed = owned.borrowed();
    assert_eq!(borrowed.endpoint, "/a");
    assert_eq!(borrowed.status, Status::Success);
    // `Copy`, so using the view does not consume it.
    let copy = borrowed;
    assert_eq!(copy.endpoint, borrowed.endpoint);

    let optional = Optional {
        endpoint: Endpoint::from("/a"),
        region: None,
    };
    assert_eq!(optional.borrowed().region, None);
    let optional = Optional {
        endpoint: Endpoint::from("/a"),
        region: Some(Region::from("eu")),
    };
    assert_eq!(optional.borrowed().region, Some("eu"));
}
