use std::net::UdpSocket;
use std::time::Duration;

use chalk_metrics::client;
use chalk_metrics::export::prometheus::PrometheusExporter;
use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter};

mod common;

#[test]
fn raw_timer_recycles_queue_without_flushing_aggregates_on_either_runtime() {
    for threads in [1, 2] {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let prom = PrometheusExporter::builder().build();
        let text = prom.text_handle();
        let exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string())
            .histogram_mode(HistogramExportMode::Distribution)
            .build()
            .unwrap();
        let client = client::builder()
            .with_exporter(exporter)
            .with_exporter(prom)
            .worker_threads(threads)
            .raw_sample_queue_capacity(2)
            .raw_sample_flush_interval(Duration::from_millis(10))
            .flush_interval(Duration::from_secs(3600))
            .build_local();
        client.record_count("requests", &[], 1, Vec::new, 7);
        let mut buffer = [0; 4096];
        for _ in 0..4 {
            for value in [1.0, 99.0] {
                client.record_histogram("latency", &[], 42, Vec::new, value);
            }
            let mut received = Vec::new();
            while received.len() < 2 {
                let n = socket.recv(&mut buffer).unwrap();
                received.extend(common::expand_lines(
                    std::str::from_utf8(&buffer[..n]).unwrap(),
                ));
            }
            received.sort();
            assert_eq!(received, ["latency:1|d", "latency:99|d"]);
            assert!(
                text.read().is_empty(),
                "raw timer must not consume sketches"
            );
        }
        assert_eq!(client.dropped_raw_samples(), 0);
        client.shutdown();
        let n = socket.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"requests:7|c");
        assert!(text.read().contains("latency_count 8"));
        assert!(text.read().contains("latency_sum 400"));
    }
}

#[test]
fn default_raw_timer_delivers_before_long_aggregate_interval() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let client = client::builder()
        .with_exporter(
            StatsdExporter::udp(socket.local_addr().unwrap().to_string())
                .histogram_mode(HistogramExportMode::Distribution)
                .build()
                .unwrap(),
        )
        .flush_interval(Duration::from_secs(3600))
        .build_local();
    client.record_histogram("latency", &[], 42, Vec::new, 99.0);
    let mut buffer = [0; 64];
    let n = socket.recv(&mut buffer).unwrap();
    assert_eq!(&buffer[..n], b"latency:99|d");
    client.shutdown();
}

#[test]
#[should_panic(expected = "raw sample flush interval must be positive")]
fn zero_raw_interval_is_rejected() {
    client::builder().raw_sample_flush_interval(Duration::ZERO);
}
