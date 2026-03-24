#[allow(clippy::vec_init_then_push)]
mod inner {
    include!(concat!(env!("OUT_DIR"), "/metrics_generated.rs"));
}

pub use inner::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of<T: Hash>(t: &T) -> u64 {
        let mut h = DefaultHasher::new();
        t.hash(&mut h);
        h.finish()
    }

    // Tag type tests

    #[test]
    fn test_enum_tag_display() {
        assert_eq!(Status::Success.to_string(), "success");
        assert_eq!(Status::Failure.to_string(), "failure");
        assert_eq!(Status::Timeout.to_string(), "timeout");
    }

    #[test]
    fn test_string_tag_display() {
        assert_eq!(Endpoint::from("api/v1").to_string(), "api/v1");
        assert_eq!(Endpoint::from(String::from("api/v1")).as_str(), "api/v1");
    }

    // Metric struct tests

    #[test]
    fn test_metric_name_and_namespace() {
        assert_eq!(Uptime::NAME, "uptime");
        assert_eq!(Uptime::NAMESPACE, &[] as &[&str]);

        assert_eq!(HttpRequestCount::NAME, "request_count");
        assert_eq!(HttpRequestCount::NAMESPACE, &["http"]);

        assert_eq!(HttpRequestLatency::NAME, "request_latency");
        assert_eq!(HttpRequestLatency::NAMESPACE, &["http"]);

        assert_eq!(ResolverLatency::NAME, "latency");
        assert_eq!(ResolverLatency::NAMESPACE, &["resolver"]);
    }

    #[test]
    fn test_metric_struct_hash_eq() {
        let a = HttpRequestLatency {
            endpoint: Endpoint::from("api/v1"),
            status: Status::Success,
        };
        let b = HttpRequestLatency {
            endpoint: Endpoint::from("api/v1"),
            status: Status::Success,
        };
        assert_eq!(a, b);
        assert_eq!(hash_of(&a), hash_of(&b));

        let c = HttpRequestLatency {
            endpoint: Endpoint::from("api/v2"),
            status: Status::Success,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn test_optional_tag() {
        let tags = HttpActiveConnections {
            endpoint: Endpoint::from("api/v1"),
            region: None,
        };
        let pairs = tags.export_pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "endpoint");
        assert_eq!(pairs[0].1.as_ref(), "api/v1");

        let tags_with = HttpActiveConnections {
            endpoint: Endpoint::from("api/v1"),
            region: Some(Region::from("us-east-1")),
        };
        assert_eq!(tags_with.export_pairs().len(), 2);
    }

    #[test]
    fn test_export_name_override() {
        let tags = ResolverLatency {
            endpoint: Endpoint::from("api/v1"),
            resolver_status: Status::Success,
            resolver_fqn: None,
        };
        let pairs = tags.export_pairs();
        assert_eq!(pairs[1].0, "resolver_status");
    }

    #[test]
    fn test_top_level_metric_empty_tags() {
        let tags = Uptime {};
        let pairs = tags.export_pairs();
        assert!(pairs.is_empty());
    }

    #[test]
    fn test_record_count_compiles() {
        // record() with no arg → increment by 1
        HttpRequestCount {
            endpoint: Endpoint::from("/api"),
            status: Status::Success,
        }
        .record();

        // record_value() with explicit delta
        HttpRequestCount {
            endpoint: Endpoint::from("/api"),
            status: Status::Success,
        }
        .record_value(5);
    }

    #[test]
    fn test_record_gauge_compiles() {
        Uptime {}.record(42.0);
    }

    #[test]
    fn test_record_histogram_compiles() {
        HttpRequestLatency {
            endpoint: Endpoint::from("/api"),
            status: Status::Success,
        }
        .record(0.042);
    }

    #[test]
    fn test_record_macro() {
        // Count with no value (increment by 1)
        record!(HttpRequestCount {
            endpoint: Endpoint::from("/api"),
            status: Status::Success,
        });

        // Gauge with value
        record!(42.0, Uptime {});

        // Histogram with value
        record!(
            0.042,
            HttpRequestLatency {
                endpoint: Endpoint::from("/api"),
                status: Status::Success,
            }
        );
    }
}
