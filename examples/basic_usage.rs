use std::time::Duration;

use chalk_metrics::export::prometheus::PrometheusExporter;
use chalk_metrics::generated::*;

fn main() {
    // Set up a Prometheus exporter
    let prom = PrometheusExporter::builder()
        .namespace("myapp")
        .build();
    let text_handle = prom.text_handle();

    // Initialize the metrics client with a short flush interval for demo
    chalk_metrics::client::builder()
        .with_exporter(prom)
        .flush_interval(Duration::from_millis(100))
        .init();

    // Record some metrics using the macros
    for i in 0..10 {
        chalk_metrics::count!(
            RequestCount,
            1,
            RequestCountTags {
                endpoint: Endpoint::from("/api/v1"),
                status: Status::Success,
            }
        );

        chalk_metrics::histogram!(
            RequestLatency,
            0.01 * (i as f64 + 1.0),
            RequestLatencyTags {
                endpoint: Endpoint::from("/api/v1"),
                status: Status::Success,
            }
        );
    }

    chalk_metrics::gauge!(
        ActiveConnections,
        42.0,
        ActiveConnectionsTags {
            endpoint: Endpoint::from("/api/v1"),
            region: Some(Region::from("us-east-1")),
        }
    );

    // Wait for a flush to happen
    std::thread::sleep(Duration::from_millis(200));

    // Read the Prometheus text output
    let text = text_handle.read().clone();
    println!("=== Prometheus Output ===\n{text}");

    // Shutdown cleanly
    chalk_metrics::client::shutdown();
}
