use std::net::UdpSocket;
use std::time::Duration;

use chalk_metrics::client;
use chalk_metrics::export::prometheus::PrometheusExporter;
use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter};

mod common;

fn listener() -> UdpSocket {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    socket
}

fn lines(socket: &UdpSocket) -> Vec<String> {
    let mut lines = Vec::new();
    let mut buffer = [0; 65536];
    loop {
        match socket.recv(&mut buffer) {
            Ok(n) => lines.extend(common::expand_lines(
                std::str::from_utf8(&buffer[..n]).unwrap(),
            )),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => panic!("UDP receive failed: {error}"),
        }
    }
    lines.sort();
    lines
}

fn distribution(socket: &UdpSocket) -> StatsdExporter {
    StatsdExporter::udp(socket.local_addr().unwrap().to_string())
        .histogram_mode(HistogramExportMode::Distribution)
        .build()
        .unwrap()
}

#[test]
fn distribution_preserves_observations_with_default_initialization() {
    let socket = listener();
    let client = client::builder()
        .with_exporter(distribution(&socket))
        .flush_interval(Duration::from_secs(3600))
        .build_local();
    client.record_histogram("latency", &[], 42, Vec::new, 1.0);
    client.record_histogram("latency", &[], 42, Vec::new, 99.0);
    client.shutdown();
    client.shutdown();
    assert_eq!(lines(&socket), ["latency:1|d", "latency:99|d"]);
}

#[test]
fn overflow_drops_newest_raw_sample_but_retains_complete_sketch() {
    let socket = listener();
    let prom = PrometheusExporter::builder().build();
    let text = prom.text_handle();
    let client = client::builder()
        .with_exporter(distribution(&socket))
        .with_exporter(prom)
        .raw_sample_queue_capacity(2)
        .flush_interval(Duration::from_secs(3600))
        .build_local();
    for value in [1.0, 99.0, 5.0] {
        client.record_histogram("latency", &[], 42, Vec::new, value);
    }
    assert_eq!(client.dropped_raw_samples(), 1);
    client.shutdown();
    assert_eq!(lines(&socket), ["latency:1|d", "latency:99|d"]);
    assert!(text.read().contains("latency_count 3"));
    assert!(text.read().contains("latency_sum 105"));
    client.record_histogram("latency", &[], 42, Vec::new, 7.0);
    assert_eq!(
        client.dropped_raw_samples(),
        2,
        "closed queue rejects post-shutdown raw samples"
    );
}

#[test]
#[should_panic(expected = "raw sample queue capacity must be positive")]
fn zero_raw_queue_capacity_is_rejected() {
    client::builder().raw_sample_queue_capacity(0);
}

#[test]
#[allow(deprecated)]
fn mixed_exporters_keep_samples_and_sketches_with_either_legacy_setting() {
    for legacy in [true, false] {
        let first = listener();
        let second = listener();
        let percentiles = listener();
        let prom = PrometheusExporter::builder().build();
        let text = prom.text_handle();
        let client = client::builder()
            .with_exporter(distribution(&first))
            .with_exporter(distribution(&second))
            .with_exporter(
                StatsdExporter::udp(percentiles.local_addr().unwrap().to_string())
                    .histogram_mode(HistogramExportMode::Percentiles(vec![0.5, 0.99]))
                    .build()
                    .unwrap(),
            )
            .with_exporter(prom)
            .aggregate_distr_metrics(legacy)
            .flush_interval(Duration::from_secs(3600))
            .build_local();
        for value in [1.0, 99.0] {
            client.record_histogram("latency", &[], 42, Vec::new, value);
        }
        client.record_count("requests", &[], 2, Vec::new, 7);
        client.record_gauge("active", &[], 3, Vec::new, 4.0);
        client.shutdown();
        let expected = ["active:4|g", "latency:1|d", "latency:99|d", "requests:7|c"];
        assert_eq!(lines(&first), expected);
        assert_eq!(lines(&second), expected);
        let percentile_lines = lines(&percentiles);
        assert!(percentile_lines.contains(&"latency.count:2|g".to_owned()));
        assert!(percentile_lines.contains(&"latency.avg:50|g".to_owned()));
        assert_eq!(percentile_lines.len(), 6);
        assert!(percentile_lines.iter().all(|line| !line.contains("|d")));
        assert!(text.read().contains("latency_count 2"));
        assert!(text.read().contains("latency_sum 100"));
    }
}
