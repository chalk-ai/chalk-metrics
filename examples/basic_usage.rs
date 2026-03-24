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

    // Record namespaced count metrics (increment by 1)
    for _ in 0..10 {
        HttpRequestCount {
            endpoint: Endpoint::from("/api/v1"),
            status: Status::Success,
        }
        .record();
    }

    // Record namespaced histogram
    for i in 0..10 {
        HttpRequestLatency {
            endpoint: Endpoint::from("/api/v1"),
            status: Status::Success,
        }
        .record(0.01 * (i as f64 + 1.0));
    }

    // Top-level gauge (no namespace)
    Uptime {}.record(42.0);

    // Resolver namespace histogram with export_name overrides
    ResolverLatency {
        endpoint: Endpoint::from("my.resolver"),
        resolver_status: Status::Success,
        resolver_fqn: Some(Endpoint::from("my.resolver.fqn")),
    }
    .record(0.005);

    std::thread::sleep(Duration::from_millis(200));

    let text = text_handle.read().clone();
    println!("=== Prometheus Output ===\n{text}");

    chalk_metrics::client::shutdown();
}
