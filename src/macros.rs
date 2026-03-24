/// Record a count delta for a metric.
///
/// The metric is identified by its `MetricId` variant, and tags are provided
/// as the corresponding tags struct. The tags struct is hashed for the
/// aggregation map key, and `export_pairs()` is only called on the cold path
/// (first insertion).
///
/// # Example
///
/// ```rust,ignore
/// use chalk_metrics::generated::*;
///
/// chalk_metrics::count!(RequestCount, 1, RequestCountTags {
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
/// chalk_metrics::gauge!(ActiveConnections, 42.0, ActiveConnectionsTags {
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
/// chalk_metrics::histogram!(RequestLatency, 0.042, RequestLatencyTags {
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
        // Verify the macro compiles and runs without panic (no-op since
        // global client isn't initialized in unit tests)
        count!(
            RequestCount,
            1,
            RequestCountTags {
                endpoint: Endpoint::from("/api"),
                status: Status::Success,
            }
        );
    }

    #[test]
    fn test_gauge_macro() {
        gauge!(
            ActiveConnections,
            42.0,
            ActiveConnectionsTags {
                endpoint: Endpoint::from("/api"),
                region: None,
            }
        );
    }

    #[test]
    fn test_histogram_macro() {
        histogram!(
            RequestLatency,
            0.042,
            RequestLatencyTags {
                endpoint: Endpoint::from("/api"),
                status: Status::Success,
            }
        );
    }

    #[test]
    fn test_macro_with_optional_tag() {
        gauge!(
            ActiveConnections,
            10.0,
            ActiveConnectionsTags {
                endpoint: Endpoint::from("/api"),
                region: Some(Region::from("us-east-1")),
            }
        );
    }

    #[test]
    fn test_macro_with_local_client() {
        use crate::client;
        use std::time::Duration;

        let client = client::builder()
            .flush_interval(Duration::from_secs(60))
            .build_local();

        // Record directly on the local client
        client.record_count(
            MetricId::RequestCount as u16,
            "request_count",
            0,
            || vec![("endpoint", "/api".into()), ("status", "success".into())],
            5,
        );

        let flushed = client.flush();
        assert_eq!(flushed.len(), 1);

        client.shutdown();
    }
}
