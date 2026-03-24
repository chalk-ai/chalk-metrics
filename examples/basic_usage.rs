use std::time::Duration;

use chalk_metrics::export::prometheus::PrometheusExporter;
use chalk_metrics::generated::*;

fn main() {
    let prom = PrometheusExporter::builder().namespace("myapp").build();
    let text_handle = prom.text_handle();

    chalk_metrics::client::builder()
        .with_exporter(prom)
        .flush_interval(Duration::from_millis(100))
        .init();

    // Record namespaced metrics
    for i in 0..10 {
        chalk_metrics::count!(
            HttpRequestCount,
            1,
            HttpRequestCountTags {
                endpoint: Endpoint::from("/api/v1"),
                status: Status::Success,
            }
        );

        chalk_metrics::histogram!(
            HttpRequestLatency,
            0.01 * (i as f64 + 1.0),
            HttpRequestLatencyTags {
                endpoint: Endpoint::from("/api/v1"),
                status: Status::Success,
            }
        );
    }

    // Record a top-level metric (no namespace)
    chalk_metrics::gauge!(Uptime, 42.0, UptimeTags {});

    // Record a metric in the resolver namespace
    chalk_metrics::histogram!(
        ResolverLatency,
        0.005,
        ResolverLatencyTags {
            endpoint: Endpoint::from("my.resolver"),
            resolver_status: Status::Success,
            resolver_fqn: Some(Endpoint::from("my.resolver.fqn")),
        }
    );

    std::thread::sleep(Duration::from_millis(200));

    let text = text_handle.read().clone();
    println!("=== Prometheus Output ===\n{text}");

    chalk_metrics::client::shutdown();
}
