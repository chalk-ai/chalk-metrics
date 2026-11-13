use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

use chalk_metrics::export::statsd::{HistogramExportMode, StatsdExporter};
use chalk_metrics::export::{Exporter, RawDistributionPoint, TagsData};

mod common;

fn point(series: &'static str, tag: &'static str, value: f64) -> RawDistributionPoint {
    RawDistributionPoint {
        namespace: &[],
        metric_name: series,
        tags: Arc::new(TagsData {
            pairs: vec![("series", tag.into())],
        }),
        value,
    }
}

async fn export(points: &[RawDistributionPoint], packing: bool, limit: usize) -> Vec<String> {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let exporter = StatsdExporter::udp(socket.local_addr().unwrap().to_string())
        .histogram_mode(HistogramExportMode::Distribution)
        .distribution_value_packing(packing)
        .max_buffer_size(limit)
        .build()
        .unwrap();
    exporter.export_raw_points(points).await.unwrap();
    let mut packets = Vec::new();
    let mut buffer = [0; 65536];
    loop {
        match socket.recv(&mut buffer) {
            Ok(n) => {
                assert!(n <= limit);
                packets.push(std::str::from_utf8(&buffer[..n]).unwrap().to_owned());
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(e) => panic!("UDP receive failed: {e}"),
        }
    }
    packets
}

#[tokio::test]
async fn packing_preserves_interleaved_series_tags_namespaces_and_repeated_values() {
    let mut namespaced = point("latency", "a", 7.0);
    namespaced.namespace = &["http"];
    let points = [
        point("latency", "a", 1.0),
        point("latency", "b", 3.0),
        point("latency", "a", 99.0),
        namespaced,
        point("other", "a", -5.0),
        point("latency", "a", 99.0),
    ];
    let packed = export(&points, true, 1432).await;
    assert!(packed.join("\n").contains("latency:1:99:99|d|#series:a"));
    let mut decoded = common::expand_lines(&packed.join("\n"));
    let mut unpacked = common::expand_lines(&export(&points, false, 1432).await.join("\n"));
    decoded.sort();
    unpacked.sort();
    assert_eq!(decoded, unpacked);
    assert_eq!(decoded.len(), points.len());
}

#[tokio::test]
async fn packing_splits_at_exact_byte_limit_including_tags_and_utf8() {
    let points = [
        point("x", "é", 1.0),
        point("x", "é", 99.0),
        point("x", "é", 333.0),
    ];
    // x:1:99|d|#series:é occupies exactly 19 UTF-8 bytes.
    let packets = export(&points, true, 19).await;
    assert_eq!(packets, ["x:1:99|d|#series:é", "x:333|d|#series:é"]);
    assert_eq!(common::expand_lines(&packets.join("\n")).len(), 3);
}

#[tokio::test]
async fn packing_can_be_disabled_for_older_agents() {
    assert_eq!(
        export(&[point("x", "a", 1.0), point("x", "a", 99.0)], false, 1432).await,
        ["x:1|d|#series:a\nx:99|d|#series:a"]
    );
}
