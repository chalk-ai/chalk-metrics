#[allow(clippy::vec_init_then_push)]
mod inner {
    include!(concat!(env!("OUT_DIR"), "/metrics_generated.rs"));
}

pub use inner::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of<T: Hash>(t: &T) -> u64 {
        let mut h = DefaultHasher::new();
        t.hash(&mut h);
        h.finish()
    }

    #[test]
    fn test_enum_tag_display() {
        assert_eq!(Status::Success.to_string(), "success");
        assert_eq!(Status::Failure.to_string(), "failure");
        assert_eq!(Status::Timeout.to_string(), "timeout");
    }

    #[test]
    fn test_enum_tag_as_str() {
        assert_eq!(Status::Success.as_str(), "success");
    }

    #[test]
    fn test_string_tag_display() {
        let e = Endpoint::from("api/v1");
        assert_eq!(e.to_string(), "api/v1");
    }

    #[test]
    fn test_string_tag_from_string() {
        let e = Endpoint::from(String::from("api/v1"));
        assert_eq!(e.as_str(), "api/v1");
    }

    #[test]
    fn test_metric_id_name() {
        // name() returns just the metric name, not namespace-prefixed
        assert_eq!(MetricId::Uptime.name(), "uptime");
        assert_eq!(MetricId::HttpRequestLatency.name(), "request_latency");
        assert_eq!(MetricId::HttpRequestCount.name(), "request_count");
        assert_eq!(MetricId::HttpActiveConnections.name(), "active_connections");
        assert_eq!(MetricId::ResolverLatency.name(), "latency");
    }

    #[test]
    fn test_metric_id_namespace() {
        assert_eq!(MetricId::Uptime.namespace(), &[] as &[&str]);
        assert_eq!(MetricId::HttpRequestLatency.namespace(), &["http"]);
        assert_eq!(MetricId::HttpRequestCount.namespace(), &["http"]);
        assert_eq!(MetricId::ResolverLatency.namespace(), &["resolver"]);
    }

    #[test]
    fn test_metric_id_type() {
        assert_eq!(MetricId::HttpRequestLatency.metric_type(), MetricType::Histogram);
        assert_eq!(MetricId::HttpRequestCount.metric_type(), MetricType::Count);
        assert_eq!(MetricId::HttpActiveConnections.metric_type(), MetricType::Gauge);
        assert_eq!(MetricId::Uptime.metric_type(), MetricType::Gauge);
    }

    #[test]
    fn test_metric_id_description() {
        assert_eq!(
            MetricId::HttpRequestLatency.description(),
            "HTTP request latency in seconds"
        );
    }

    #[test]
    fn test_metric_id_display() {
        assert_eq!(MetricId::HttpRequestLatency.to_string(), "request_latency");
    }

    #[test]
    fn test_metric_id_repr() {
        // Order: top-level first, then namespaces sorted alphabetically
        assert_eq!(MetricId::Uptime as u16, 0);
        assert_eq!(MetricId::HttpRequestLatency as u16, 1);
        assert_eq!(MetricId::HttpRequestCount as u16, 2);
        assert_eq!(MetricId::HttpActiveConnections as u16, 3);
        assert_eq!(MetricId::ResolverLatency as u16, 4);
    }

    #[test]
    fn test_all_metrics_constant() {
        assert_eq!(ALL_METRICS.len(), 5);
        assert_eq!(ALL_METRICS[0], MetricId::Uptime);
        assert_eq!(ALL_METRICS[1], MetricId::HttpRequestLatency);
    }

    #[test]
    fn test_tags_struct_hash_eq() {
        let a = HttpRequestLatencyTags {
            endpoint: Endpoint::from("api/v1"),
            status: Status::Success,
        };
        let b = HttpRequestLatencyTags {
            endpoint: Endpoint::from("api/v1"),
            status: Status::Success,
        };
        assert_eq!(a, b);
        assert_eq!(hash_of(&a), hash_of(&b));

        let c = HttpRequestLatencyTags {
            endpoint: Endpoint::from("api/v2"),
            status: Status::Success,
        };
        assert_ne!(a, c);
        assert_ne!(hash_of(&a), hash_of(&c));
    }

    #[test]
    fn test_optional_tag_none() {
        let tags = HttpActiveConnectionsTags {
            endpoint: Endpoint::from("api/v1"),
            region: None,
        };
        let pairs = tags.export_pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], ("endpoint", "api/v1".to_string()));
    }

    #[test]
    fn test_optional_tag_some() {
        let tags = HttpActiveConnectionsTags {
            endpoint: Endpoint::from("api/v1"),
            region: Some(Region::from("us-east-1")),
        };
        let pairs = tags.export_pairs();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[1], ("region", "us-east-1".to_string()));
    }

    #[test]
    fn test_export_name_override() {
        let tags = ResolverLatencyTags {
            endpoint: Endpoint::from("api/v1"),
            resolver_status: Status::Success,
            resolver_fqn: None,
        };
        let pairs = tags.export_pairs();
        assert_eq!(pairs[1].0, "resolver_status");
        assert_eq!(pairs[1].1, "success");
    }

    #[test]
    fn test_top_level_metric_no_namespace() {
        let tags = UptimeTags {};
        let pairs = tags.export_pairs();
        assert!(pairs.is_empty());
    }
}
