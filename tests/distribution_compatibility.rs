#![allow(deprecated)]
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

use chalk_metrics::client;
use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter};
use chalk_metrics::export::{Exporter, FlushedMetric, FlushedValue, TagsData, UDDSketch};

mod common;

fn listener() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    socket
}

fn lines(socket: &UdpSocket) -> Vec<String> {
    let mut result = Vec::new();
    let mut buffer = [0; 65536];
    loop {
        match socket.recv(&mut buffer) {
            Ok(n) => result.extend(common::expand_lines(
                std::str::from_utf8(&buffer[..n]).unwrap(),
            )),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(e) => panic!("receive failed: {e}"),
        }
    }
    result.sort();
    result
}

#[test]
fn legacy_false_preserves_default_statsd_distributions_and_explicit_modes_win() {
    for legacy in [None, Some(true), Some(false)] {
        for explicit in [None, Some(false), Some(true)] {
            let socket = listener();
            let mut exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string());
            if let Some(distribution) = explicit {
                exporter = exporter.histogram_mode(if distribution {
                    HistogramExportMode::Distribution
                } else {
                    HistogramExportMode::Percentiles(vec![0.5, 0.99])
                });
            }
            let mut builder = client::builder()
                .with_exporter(exporter.build().unwrap())
                .flush_interval(Duration::from_secs(3600));
            if let Some(legacy) = legacy {
                builder = builder.aggregate_distr_metrics(legacy);
            }
            let client = builder.build_local();
            for value in [1.0, 99.0] {
                client.record_histogram("latency", &[], 42, Vec::new, value);
            }
            client.record_count("requests", &[], 43, Vec::new, 7);
            client.record_gauge("active", &[], 44, Vec::new, 4.0);
            client.shutdown();
            let actual = lines(&socket);
            let distribution = explicit.unwrap_or(legacy == Some(false));
            if distribution {
                assert_eq!(
                    actual,
                    ["active:4|g", "latency:1|d", "latency:99|d", "requests:7|c"],
                    "legacy={legacy:?}, explicit={explicit:?}"
                );
            } else {
                assert!(actual.contains(&"latency.count:2|g".into()));
                assert!(actual.contains(&"latency.avg:50|g".into()));
                assert!(actual.contains(&"active:4|g".into()));
                assert!(actual.contains(&"requests:7|c".into()));
                assert!(actual.iter().all(|line| !line.contains("|d")));
                assert_eq!(actual.len(), if explicit.is_some() { 6 } else { 8 });
            }
        }
    }
}

fn metric(value: FlushedValue, name: &'static str) -> FlushedMetric {
    FlushedMetric {
        namespace: &[],
        metric_name: name,
        tags: Arc::new(TagsData { pairs: vec![] }),
        value,
    }
}
fn histogram() -> FlushedMetric {
    let mut sketch = UDDSketch::new(200, 0.001);
    sketch.add_value(1.0);
    sketch.add_value(99.0);
    metric(FlushedValue::Histogram(sketch), "latency")
}

#[tokio::test]
async fn incompatible_sketch_does_not_drop_valid_siblings_in_any_position() {
    for position in 0..3 {
        let socket = listener();
        let exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string())
            .histogram_mode(HistogramExportMode::Distribution)
            .build()
            .unwrap();
        let mut batch = vec![
            metric(FlushedValue::Count(7), "requests"),
            metric(FlushedValue::Gauge(4.0), "active"),
        ];
        batch.insert(position, histogram());
        let error = exporter.export(&batch).await.unwrap_err();
        assert!(error.message.contains("original samples"));
        assert_eq!(lines(&socket), ["active:4|g", "requests:7|c"]);
        exporter
            .export(&[metric(FlushedValue::Count(1), "next")])
            .await
            .unwrap();
        assert_eq!(lines(&socket), ["next:1|c"]);
    }
}

#[tokio::test]
async fn all_incompatible_sketches_report_error_without_substituting_means() {
    let socket = listener();
    let exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string())
        .histogram_mode(HistogramExportMode::Distribution)
        .build()
        .unwrap();
    assert!(exporter.export(&[histogram(), histogram()]).await.is_err());
    assert!(lines(&socket).is_empty());
    exporter.export(&[]).await.unwrap();
}

#[test]
fn legacy_false_default_statsd_and_prometheus_preserve_both_representations() {
    let socket = listener();
    let prom = chalk_metrics::export::prometheus::PrometheusExporter::builder().build();
    let text = prom.text_handle();
    // Setting the toggle before registering exporters must also work.
    let client = client::builder()
        .aggregate_distr_metrics(false)
        .with_exporter(
            StatsdExporter::udp(socket.local_addr().unwrap().to_string())
                .build()
                .unwrap(),
        )
        .with_exporter(prom)
        .flush_interval(Duration::from_secs(3600))
        .build_local();
    for value in [1.0, 99.0] {
        client.record_histogram("latency", &[], 42, Vec::new, value);
    }
    client.shutdown();
    assert_eq!(lines(&socket), ["latency:1|d", "latency:99|d"]);
    assert!(text.read().contains("latency_count 2"));
    assert!(text.read().contains("latency_sum 100"));
}
