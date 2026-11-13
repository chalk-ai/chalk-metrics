use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter};
use chalk_metrics::export::{Exporter, RawDistributionPoint, TagsData};

fn point(name: &'static str, value: f64) -> RawDistributionPoint {
    RawDistributionPoint {
        namespace: &[],
        metric_name: name,
        tags: Arc::new(TagsData { pairs: vec![] }),
        value,
    }
}

fn packets(socket: &UdpSocket) -> Vec<Vec<u8>> {
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let mut packets = Vec::new();
    let mut buffer = [0; 65536];
    loop {
        match socket.recv(&mut buffer) {
            Ok(n) => packets.push(buffer[..n].to_vec()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => panic!("receive failed: {error}"),
        }
    }
    packets
}

#[tokio::test]
async fn udp_supports_ipv4_and_ipv6() {
    for address in ["127.0.0.1:0", "[::1]:0"] {
        let socket = UdpSocket::bind(address).unwrap();
        let exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string())
            .histogram_mode(HistogramExportMode::Distribution)
            .build()
            .unwrap();
        exporter
            .export_raw_points(&[point("latency", 1.0), point("latency", 99.0)])
            .await
            .unwrap();
        assert_eq!(packets(&socket), [b"latency:1:99|d".to_vec()]);
    }
}

#[tokio::test]
async fn localhost_reaches_an_ipv4_only_agent() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let exporter =
        StatsdExporter::udp(format!("localhost:{}", socket.local_addr().unwrap().port()))
            .histogram_mode(HistogramExportMode::Distribution)
            .build()
            .unwrap();
    exporter
        .export_raw_points(&[point("latency", 1.0), point("latency", 99.0)])
        .await
        .unwrap();
    assert_eq!(packets(&socket), [b"latency:1:99|d".to_vec()]);
}

#[tokio::test]
async fn batching_counts_newlines_and_preserves_every_sample() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string())
        .histogram_mode(HistogramExportMode::Distribution)
        .distribution_value_packing(false)
        .max_buffer_size(12)
        .build()
        .unwrap();
    exporter
        .export_raw_points(&[point("x", 1.0), point("x", 99.0), point("x", 3.0)])
        .await
        .unwrap();
    assert_eq!(
        packets(&socket),
        [b"x:1|d\nx:99|d".to_vec(), b"x:3|d".to_vec()]
    );
}

#[tokio::test]
async fn oversized_lines_are_rejected_without_losing_valid_siblings() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string())
        .histogram_mode(HistogramExportMode::Distribution)
        .max_buffer_size(12)
        .build()
        .unwrap();
    let result = exporter
        .export_raw_points(&[
            point("x", 1.0),
            point("oversized_metric_name", 42.0),
            point("x", 99.0),
        ])
        .await;
    assert!(result.is_err());
    assert_eq!(packets(&socket), [b"x:1:99|d".to_vec()]);
}

#[test]
fn zero_datagram_limit_is_rejected() {
    assert!(
        StatsdExporter::udp("127.0.0.1:8125")
            .max_buffer_size(0)
            .build()
            .is_err()
    );
}

#[tokio::test]
async fn line_limits_count_utf8_tag_bytes_and_namespace() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string())
        .namespace("app")
        .default_tag("env", "prod")
        .histogram_mode(HistogramExportMode::Distribution)
        .max_buffer_size(28)
        .build()
        .unwrap();
    let sample = RawDistributionPoint {
        namespace: &["http"],
        metric_name: "x",
        tags: Arc::new(TagsData {
            pairs: vec![("t", "é".into())],
        }),
        value: 1.0,
    };
    // 29 UTF-8 bytes, although there are only 28 Unicode scalar values.
    assert!(exporter.export_raw_points(&[sample]).await.is_err());
    assert!(packets(&socket).is_empty());
}

#[cfg(unix)]
#[tokio::test]
async fn uds_reconnects_after_socket_replacement() {
    use std::os::unix::net::UnixDatagram;
    let directory = std::env::temp_dir();
    let path = directory.join(format!("chalk-metrics-{}-first.sock", std::process::id()));
    let replacement = directory.join(format!(
        "chalk-metrics-{}-replacement.sock",
        std::process::id()
    ));
    let first = UnixDatagram::bind(&path).unwrap();
    let second = UnixDatagram::bind(&replacement).unwrap();
    first
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let exporter = StatsdExporter::uds(&path)
        .histogram_mode(HistogramExportMode::Distribution)
        .build()
        .unwrap();
    let mut buffer = [0; 64];
    exporter
        .export_raw_points(&[point("x", 1.0)])
        .await
        .unwrap();
    let n = first.recv(&mut buffer).unwrap();
    assert_eq!(&buffer[..n], b"x:1|d");
    std::fs::rename(&replacement, &path).unwrap();
    exporter
        .export_raw_points(&[point("x", 99.0)])
        .await
        .unwrap();
    let n = second.recv(&mut buffer).unwrap();
    assert_eq!(&buffer[..n], b"x:99|d");
    std::fs::remove_file(path).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn missing_uds_destination_reports_an_error() {
    let path = format!("/tmp/chalk-metrics-missing-{}.sock", std::process::id());
    let exporter = StatsdExporter::uds(path)
        .histogram_mode(HistogramExportMode::Distribution)
        .build()
        .unwrap();
    assert!(
        exporter
            .export_raw_points(&[point("x", 1.0)])
            .await
            .is_err()
    );
}
