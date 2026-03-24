/// Record a count delta for a metric.
///
/// # Example
///
/// ```rust,ignore
/// use chalk_metrics::generated::*;
///
/// chalk_metrics::count!(HttpRequestCount, 1, HttpRequestCountTags {
///     endpoint: Endpoint::from("/api"),
///     status: Status::Success,
/// });
/// ```
#[macro_export]
macro_rules! count {
    ($metric:ident, $value:expr, $tags:expr) => {{
        let tags = $tags;
        let hash = {
            use ::std::hash::{Hash, Hasher};
            let mut hasher = ::std::collections::hash_map::DefaultHasher::new();
            tags.hash(&mut hasher);
            hasher.finish()
        };
        $crate::client::record_count(
            $crate::generated::MetricId::$metric as u16,
            $crate::generated::MetricId::$metric.name(),
            $crate::generated::MetricId::$metric.namespace(),
            hash,
            || tags.export_pairs(),
            $value,
        );
    }};
}

/// Record a gauge value for a metric.
///
/// # Example
///
/// ```rust,ignore
/// chalk_metrics::gauge!(HttpActiveConnections, 42.0, HttpActiveConnectionsTags {
///     endpoint: Endpoint::from("/api"),
///     region: None,
/// });
/// ```
#[macro_export]
macro_rules! gauge {
    ($metric:ident, $value:expr, $tags:expr) => {{
        let tags = $tags;
        let hash = {
            use ::std::hash::{Hash, Hasher};
            let mut hasher = ::std::collections::hash_map::DefaultHasher::new();
            tags.hash(&mut hasher);
            hasher.finish()
        };
        $crate::client::record_gauge(
            $crate::generated::MetricId::$metric as u16,
            $crate::generated::MetricId::$metric.name(),
            $crate::generated::MetricId::$metric.namespace(),
            hash,
            || tags.export_pairs(),
            $value,
        );
    }};
}

/// Record a histogram value for a metric.
///
/// # Example
///
/// ```rust,ignore
/// chalk_metrics::histogram!(HttpRequestLatency, 0.042, HttpRequestLatencyTags {
///     endpoint: Endpoint::from("/api"),
///     status: Status::Success,
/// });
/// ```
#[macro_export]
macro_rules! histogram {
    ($metric:ident, $value:expr, $tags:expr) => {{
        let tags = $tags;
        let hash = {
            use ::std::hash::{Hash, Hasher};
            let mut hasher = ::std::collections::hash_map::DefaultHasher::new();
            tags.hash(&mut hasher);
            hasher.finish()
        };
        $crate::client::record_histogram(
            $crate::generated::MetricId::$metric as u16,
            $crate::generated::MetricId::$metric.name(),
            $crate::generated::MetricId::$metric.namespace(),
            hash,
            || tags.export_pairs(),
            $value,
        );
    }};
}

#[cfg(test)]
mod tests {
    use crate::generated::*;

    #[test]
    fn test_count_macro() {
        count!(
            HttpRequestCount,
            1,
            HttpRequestCountTags {
                endpoint: Endpoint::from("/api"),
                status: Status::Success,
            }
        );
    }

    #[test]
    fn test_gauge_macro() {
        gauge!(
            HttpActiveConnections,
            42.0,
            HttpActiveConnectionsTags {
                endpoint: Endpoint::from("/api"),
                region: None,
            }
        );
    }

    #[test]
    fn test_histogram_macro() {
        histogram!(
            HttpRequestLatency,
            0.042,
            HttpRequestLatencyTags {
                endpoint: Endpoint::from("/api"),
                status: Status::Success,
            }
        );
    }

    #[test]
    fn test_macro_with_optional_tag() {
        gauge!(
            HttpActiveConnections,
            10.0,
            HttpActiveConnectionsTags {
                endpoint: Endpoint::from("/api"),
                region: Some(Region::from("us-east-1")),
            }
        );
    }

    #[test]
    fn test_top_level_metric() {
        gauge!(
            Uptime,
            123.0,
            UptimeTags {}
        );
    }

    #[test]
    fn test_macro_with_local_client() {
        use crate::client;
        use std::time::Duration;

        let client = client::builder()
            .flush_interval(Duration::from_secs(60))
            .build_local();

        client.record_count(
            MetricId::HttpRequestCount as u16,
            "request_count",
            &["http"],
            0,
            || vec![("endpoint", "/api".into()), ("status", "success".into())],
            5,
        );

        let flushed = client.flush();
        assert_eq!(flushed.len(), 1);
        assert_eq!(flushed[0].namespace, &["http"]);
        assert_eq!(flushed[0].metric_name, "request_count");

        client.shutdown();
    }
}
