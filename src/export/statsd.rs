use std::borrow::Cow;
use std::io;
use std::net::{ToSocketAddrs, UdpSocket};
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::{
    DistributionInput, ExportError, Exporter, FlushedMetric, FlushedValue, RawDistributionPoint,
};

/// How histogram (UDD Sketch) data should be emitted over StatsD.
#[derive(Debug, Clone)]
pub enum HistogramExportMode {
    /// Emit summary statistics as separate gauge metrics with a `.percentile` suffix.
    /// The `Vec<f64>` contains the percentiles to emit (e.g., `[0.5, 0.9, 0.95, 0.99]`).
    /// Also emits `.count` and `.avg`.
    Percentiles(Vec<f64>),
    /// Emit as a DogStatsD distribution (`|d`). Only works with Datadog agent.
    Distribution,
}

impl Default for HistogramExportMode {
    fn default() -> Self {
        HistogramExportMode::Percentiles(vec![0.5, 0.9, 0.95, 0.99])
    }
}

/// The socket backend for sending StatsD datagrams.
enum Socket {
    Udp(UdpSocket),
    #[cfg(unix)]
    Uds {
        socket: Mutex<Option<UnixDatagram>>,
        path: PathBuf,
        last_inode: AtomicU64,
    },
}

/// StatsD/DogStatsD exporter.
///
/// Formats metrics in DogStatsD wire format and sends them over UDP or UDS.
/// Supports batching multiple metrics into a single datagram.
pub struct StatsdExporter {
    socket: Socket,
    namespace: Option<String>,
    default_tags: Vec<(String, String)>,
    histogram_mode: HistogramExportMode,
    histogram_mode_explicit: bool,
    distribution_value_packing: bool,
    max_buffer_size: usize,
    buffer: Mutex<String>,
}

/// Builder for [`StatsdExporter`].
pub struct StatsdExporterBuilder {
    target: StatsdTarget,
    namespace: Option<String>,
    default_tags: Vec<(String, String)>,
    histogram_mode: HistogramExportMode,
    histogram_mode_explicit: bool,
    distribution_value_packing: bool,
    max_buffer_size: usize,
}

enum StatsdTarget {
    Udp(String),
    #[cfg(unix)]
    Uds(PathBuf),
}

impl StatsdExporterBuilder {
    /// Set a namespace prefix (prepended to metric names with `.` separator).
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    /// Add a default tag that is appended to every metric.
    pub fn default_tag(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.default_tags.push((key.into(), value.into()));
        self
    }

    /// Set the histogram export mode.
    pub fn histogram_mode(mut self, mode: HistogramExportMode) -> Self {
        self.histogram_mode = mode;
        self.histogram_mode_explicit = true;
        self
    }

    /// Pack original Distribution values with the same series into one message
    /// (default: true). Requires DogStatsD protocol v1.1: Agent 6.25+ or 7.25+.
    /// Disable for older agents. Every observation is preserved; this is not sampling.
    pub fn distribution_value_packing(mut self, enabled: bool) -> Self {
        self.distribution_value_packing = enabled;
        self
    }

    /// Set the positive maximum datagram size in bytes (UDP default: 1432;
    /// UDS default: 8192), including newline separators. Oversized individual
    /// lines are rejected and reported while valid sibling lines are sent.
    pub fn max_buffer_size(mut self, size: usize) -> Self {
        self.max_buffer_size = size;
        self
    }

    /// Build the exporter, creating the underlying socket.
    pub fn build(self) -> io::Result<StatsdExporter> {
        if self.max_buffer_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_buffer_size must be positive",
            ));
        }
        let socket = match self.target {
            StatsdTarget::Udp(addr) => Socket::Udp(connect_udp(&addr)?),
            #[cfg(unix)]
            StatsdTarget::Uds(path) => {
                let inode = get_inode(&path).unwrap_or(0);
                let sock = connect_uds(&path).ok();
                Socket::Uds {
                    socket: Mutex::new(sock),
                    path,
                    last_inode: AtomicU64::new(inode),
                }
            }
        };

        Ok(StatsdExporter {
            socket,
            namespace: self.namespace,
            default_tags: self.default_tags,
            histogram_mode: self.histogram_mode,
            histogram_mode_explicit: self.histogram_mode_explicit,
            distribution_value_packing: self.distribution_value_packing,
            max_buffer_size: self.max_buffer_size,
            buffer: Mutex::new(String::with_capacity(self.max_buffer_size)),
        })
    }
}

impl StatsdExporter {
    /// Create a builder targeting an IPv4/IPv6 address or hostname with a port
    /// (e.g., `"127.0.0.1:8125"`, `"[::1]:8125"`, or `"localhost:8125"`).
    pub fn udp(addr: impl Into<String>) -> StatsdExporterBuilder {
        StatsdExporterBuilder {
            target: StatsdTarget::Udp(addr.into()),
            namespace: None,
            default_tags: Vec::new(),
            histogram_mode: HistogramExportMode::default(),
            histogram_mode_explicit: false,
            distribution_value_packing: true,
            max_buffer_size: 1432, // safe MTU for UDP
        }
    }

    /// Create a builder targeting a Unix domain socket path.
    #[cfg(unix)]
    pub fn uds(path: impl Into<PathBuf>) -> StatsdExporterBuilder {
        StatsdExporterBuilder {
            target: StatsdTarget::Uds(path.into()),
            namespace: None,
            default_tags: Vec::new(),
            histogram_mode: HistogramExportMode::default(),
            histogram_mode_explicit: false,
            distribution_value_packing: true,
            max_buffer_size: 8192, // UDS supports larger datagrams
        }
    }

    fn full_name(&self, namespace: &[&str], metric_name: &str) -> String {
        let mut parts = Vec::new();
        if let Some(ns) = &self.namespace {
            parts.push(ns.as_str());
        }
        parts.extend_from_slice(namespace);
        parts.push(metric_name);
        parts.join(".")
    }

    fn format_tags(&self, metric_tags: &[(&str, std::borrow::Cow<'static, str>)]) -> String {
        let mut all_tags: Vec<(&str, &str)> = self
            .default_tags
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_ref()))
            .collect();
        for (k, v) in metric_tags {
            all_tags.push((k, v.as_ref()));
        }
        if all_tags.is_empty() {
            String::new()
        } else {
            let inner: Vec<String> = all_tags.iter().map(|(k, v)| format!("{k}:{v}")).collect();
            format!("|#{}", inner.join(","))
        }
    }

    /// Format all metrics into batched DogStatsD datagrams and send them.
    fn send_metrics(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        let mut lines = Vec::new();
        let mut rejected = None;

        for metric in metrics {
            let name = self.full_name(metric.namespace, metric.metric_name);
            let tags = self.format_tags(&metric.tags.pairs);

            match &metric.value {
                FlushedValue::Count(value) => {
                    lines.push(format!("{name}:{value}|c{tags}"));
                }
                FlushedValue::Gauge(value) => {
                    lines.push(format!("{name}:{value}|g{tags}"));
                }
                FlushedValue::Histogram(sketch) => match &self.histogram_mode {
                    HistogramExportMode::Distribution => {
                        rejected.get_or_insert_with(|| ExportError::new(
                            "Distribution requires original samples; use MetricsClient or export_raw_points",
                        ));
                    }
                    HistogramExportMode::Percentiles(percentiles) => {
                        let count = sketch.count();
                        let avg = sketch.mean();
                        lines.push(format!("{name}.count:{count}|g{tags}"));
                        lines.push(format!("{name}.avg:{avg}|g{tags}"));
                        for &p in percentiles {
                            let value = sketch.estimate_quantile(p);
                            let label = format_percentile_label(p);
                            lines.push(format!("{name}.{label}:{value}|g{tags}"));
                        }
                    }
                },
            }
        }

        let result = self.send_lines(lines);
        match rejected {
            Some(error) => Err(error),
            None => result,
        }
    }

    /// Format raw distribution points into batched DogStatsD datagrams and send them.
    fn send_raw_points(&self, points: &[RawDistributionPoint]) -> Result<(), ExportError> {
        if !matches!(self.histogram_mode, HistogramExportMode::Distribution) {
            return Err(ExportError::new(
                "Percentiles requires histogram sketches, not raw distribution samples",
            ));
        }
        if self.distribution_value_packing {
            return self.send_lines(self.pack_distribution_points(points));
        }
        let lines = points.iter().map(|point| {
            let name = self.full_name(point.namespace, point.metric_name);
            let tags = self.format_tags(&point.tags.pairs);
            format_distribution_line(&name, point.value, &tags)
        });
        self.send_lines(lines)
    }

    fn pack_distribution_points(&self, points: &[RawDistributionPoint]) -> Vec<String> {
        // Compare tag contents, not Arc addresses: direct callers can supply
        // separately allocated tags, and samples can span aggregation flushes.
        type Series<'a> = (
            &'static [&'static str],
            &'static str,
            &'a [(&'static str, Cow<'static, str>)],
        );
        let mut indices = hashbrown::HashMap::<Series<'_>, usize>::new();
        let mut groups: Vec<(&RawDistributionPoint, Vec<f64>)> = Vec::new();
        for point in points {
            match indices.entry((point.namespace, point.metric_name, &point.tags.pairs)) {
                hashbrown::hash_map::Entry::Vacant(entry) => {
                    entry.insert(groups.len());
                    // Keep the first value in the point itself: unique series
                    // should not each allocate a separate values buffer.
                    groups.push((point, Vec::new()));
                }
                hashbrown::hash_map::Entry::Occupied(entry) => {
                    groups[*entry.get()].1.push(point.value);
                }
            }
        }

        let mut lines = Vec::new();
        for (point, values) in groups {
            let name = self.full_name(point.namespace, point.metric_name);
            let tags = self.format_tags(&point.tags.pairs);
            if values.is_empty() {
                lines.push(format_distribution_line(&name, point.value, &tags));
                continue;
            }
            let prefix = format!("{name}:");
            let suffix = format!("|d{tags}");
            let mut line = prefix.clone();
            for value in std::iter::once(point.value).chain(values) {
                let value = value.to_string();
                if line.len() > prefix.len()
                    && line.len() + 1 + value.len() + suffix.len() > self.max_buffer_size
                {
                    line.push_str(&suffix);
                    lines.push(std::mem::replace(&mut line, prefix.clone()));
                }
                if line.len() > prefix.len() {
                    line.push(':');
                }
                line.push_str(&value);
            }
            line.push_str(&suffix);
            lines.push(line);
        }
        lines
    }

    fn send_lines(&self, lines: impl IntoIterator<Item = String>) -> Result<(), ExportError> {
        let mut buffer = self.buffer.lock();
        buffer.clear();

        let mut rejected = None;
        for line in lines {
            if line.len() > self.max_buffer_size {
                rejected.get_or_insert_with(|| {
                    ExportError::new(format!(
                        "metric line is {} bytes, exceeding the {} byte datagram limit",
                        line.len(),
                        self.max_buffer_size
                    ))
                });
                continue;
            }
            if !buffer.is_empty() && buffer.len() + 1 + line.len() > self.max_buffer_size {
                self.send_buffer(&buffer)?;
                buffer.clear();
            }
            if !buffer.is_empty() {
                buffer.push('\n');
            }
            buffer.push_str(&line);
        }

        if !buffer.is_empty() {
            self.send_buffer(&buffer)?;
        }

        match rejected {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn send_buffer(&self, data: &str) -> Result<(), ExportError> {
        let bytes = data.as_bytes();
        let sent = match &self.socket {
            Socket::Udp(socket) => socket.send(bytes),
            #[cfg(unix)]
            Socket::Uds {
                socket,
                path,
                last_inode,
            } => {
                let current_inode = get_inode(path).unwrap_or(0);
                let mut guard = socket.lock();
                if guard.is_none()
                    || (current_inode != 0 && current_inode != last_inode.load(Ordering::Relaxed))
                {
                    *guard = Some(
                        connect_uds(path)
                            .map_err(|e| ExportError::with_source("UDS connection failed", e))?,
                    );
                    last_inode.store(current_inode, Ordering::Relaxed);
                }
                let result = guard.as_ref().expect("connected socket").send(bytes);
                if result.is_err() {
                    *guard = None;
                }
                result
            }
        }
        .map_err(|e| ExportError::with_source("metric datagram send failed", e))?;
        if sent != bytes.len() {
            return Err(ExportError::new("metric datagram was only partially sent"));
        }
        Ok(())
    }
}

fn connect_udp(destination: &str) -> io::Result<UdpSocket> {
    let mut last_error = io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "UDP destination resolved to no addresses",
    );
    for address in destination.to_socket_addrs()? {
        let bind_address = if address.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };
        let result = UdpSocket::bind(bind_address).and_then(|socket| {
            socket.connect(address)?;
            socket.set_nonblocking(true)?;
            Ok(socket)
        });
        match result {
            Ok(socket) => return Ok(socket),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

fn format_distribution_line(name: &str, value: f64, tags: &str) -> String {
    format!("{name}:{value}|d{tags}")
}

/// Format a percentile value as a label (e.g., 0.5 -> "p50", 0.99 -> "p99").
fn format_percentile_label(p: f64) -> String {
    let pct = (p * 100.0).round() as u32;
    format!("p{pct}")
}

#[cfg(unix)]
fn connect_uds(path: &Path) -> io::Result<UnixDatagram> {
    let sock = UnixDatagram::unbound()?;
    sock.connect(path)?;
    sock.set_nonblocking(true)?;
    Ok(sock)
}

#[cfg(unix)]
fn get_inode(path: &Path) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(meta.ino())
}

#[async_trait::async_trait]
impl Exporter for StatsdExporter {
    fn configure_distribution(&mut self, legacy_aggregate: bool) -> Option<DistributionInput> {
        if !legacy_aggregate && !self.histogram_mode_explicit {
            self.histogram_mode = HistogramExportMode::Distribution;
        }
        self.distribution_input()
    }

    fn distribution_input(&self) -> Option<DistributionInput> {
        Some(match self.histogram_mode {
            HistogramExportMode::Distribution => DistributionInput::Raw,
            HistogramExportMode::Percentiles(_) => DistributionInput::Sketch,
        })
    }

    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        self.send_metrics(metrics)
    }

    async fn export_raw_points(&self, points: &[RawDistributionPoint]) -> Result<(), ExportError> {
        self.send_raw_points(points)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::sketch::UDDSketch;
    use crate::aggregator::striped_map::TagsData;
    use std::sync::Arc;

    fn make_tags(pairs: Vec<(&'static str, std::borrow::Cow<'static, str>)>) -> Arc<TagsData> {
        Arc::new(TagsData { pairs })
    }

    // Helper to test formatting without actually sending
    struct FormatOnlyExporter {
        namespace: Option<String>,
        default_tags: Vec<(String, String)>,
        histogram_mode: HistogramExportMode,
    }

    impl FormatOnlyExporter {
        fn format_metric(&self, metric: &FlushedMetric) -> Vec<String> {
            let mut parts = Vec::new();
            if let Some(ns) = &self.namespace {
                parts.push(ns.as_str());
            }
            parts.extend_from_slice(metric.namespace);
            parts.push(metric.metric_name);
            let name = parts.join(".");

            let mut all_tags: Vec<(&str, &str)> = self
                .default_tags
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_ref()))
                .collect();
            for (k, v) in &metric.tags.pairs {
                all_tags.push((k, v.as_ref()));
            }
            let tags = if all_tags.is_empty() {
                String::new()
            } else {
                let inner: Vec<String> = all_tags.iter().map(|(k, v)| format!("{k}:{v}")).collect();
                format!("|#{}", inner.join(","))
            };

            let mut lines = Vec::new();
            match &metric.value {
                FlushedValue::Count(value) => {
                    lines.push(format!("{name}:{value}|c{tags}"));
                }
                FlushedValue::Gauge(value) => {
                    lines.push(format!("{name}:{value}|g{tags}"));
                }
                FlushedValue::Histogram(sketch) => match &self.histogram_mode {
                    HistogramExportMode::Distribution => {
                        let mean = sketch.mean();
                        lines.push(format!("{name}:{mean}|d{tags}"));
                    }
                    HistogramExportMode::Percentiles(percentiles) => {
                        lines.push(format!("{name}.count:{}|g{tags}", sketch.count()));
                        lines.push(format!("{name}.avg:{}|g{tags}", sketch.mean()));
                        for &p in percentiles {
                            let value = sketch.estimate_quantile(p);
                            let label = format_percentile_label(p);
                            lines.push(format!("{name}.{label}:{value}|g{tags}"));
                        }
                    }
                },
            }
            lines
        }
    }

    #[test]
    fn test_format_count() {
        let exp = FormatOnlyExporter {
            namespace: None,
            default_tags: vec![],
            histogram_mode: HistogramExportMode::default(),
        };
        let metric = FlushedMetric {
            namespace: &[],
            metric_name: "request_count",
            tags: make_tags(vec![]),
            value: FlushedValue::Count(42),
        };
        let lines = exp.format_metric(&metric);
        assert_eq!(lines, vec!["request_count:42|c"]);
    }

    #[test]
    fn test_format_gauge() {
        let exp = FormatOnlyExporter {
            namespace: None,
            default_tags: vec![],
            histogram_mode: HistogramExportMode::default(),
        };
        let metric = FlushedMetric {
            namespace: &[],
            metric_name: "temperature",
            tags: make_tags(vec![]),
            value: FlushedValue::Gauge(23.5),
        };
        let lines = exp.format_metric(&metric);
        assert_eq!(lines, vec!["temperature:23.5|g"]);
    }

    #[test]
    fn test_format_with_tags() {
        let exp = FormatOnlyExporter {
            namespace: None,
            default_tags: vec![],
            histogram_mode: HistogramExportMode::default(),
        };
        let metric = FlushedMetric {
            namespace: &[],
            metric_name: "req",
            tags: make_tags(vec![
                ("endpoint", "/api".into()),
                ("status", "success".into()),
            ]),
            value: FlushedValue::Count(1),
        };
        let lines = exp.format_metric(&metric);
        assert_eq!(lines, vec!["req:1|c|#endpoint:/api,status:success"]);
    }

    #[test]
    fn test_format_with_namespace() {
        let exp = FormatOnlyExporter {
            namespace: Some("myapp".into()),
            default_tags: vec![],
            histogram_mode: HistogramExportMode::default(),
        };
        let metric = FlushedMetric {
            namespace: &[],
            metric_name: "req",
            tags: make_tags(vec![]),
            value: FlushedValue::Count(1),
        };
        let lines = exp.format_metric(&metric);
        assert_eq!(lines, vec!["myapp.req:1|c"]);
    }

    #[test]
    fn test_format_with_default_tags() {
        let exp = FormatOnlyExporter {
            namespace: None,
            default_tags: vec![("env".into(), "prod".into())],
            histogram_mode: HistogramExportMode::default(),
        };
        let metric = FlushedMetric {
            namespace: &[],
            metric_name: "req",
            tags: make_tags(vec![("ep", "/api".into())]),
            value: FlushedValue::Count(1),
        };
        let lines = exp.format_metric(&metric);
        assert_eq!(lines, vec!["req:1|c|#env:prod,ep:/api"]);
    }

    #[test]
    fn test_format_histogram_percentiles() {
        let exp = FormatOnlyExporter {
            namespace: None,
            default_tags: vec![],
            histogram_mode: HistogramExportMode::Percentiles(vec![0.5, 0.99]),
        };

        let mut sketch = UDDSketch::new(200, 0.001);
        for i in 1..=100 {
            sketch.add_value(i as f64);
        }

        let metric = FlushedMetric {
            namespace: &[],
            metric_name: "latency",
            tags: make_tags(vec![]),
            value: FlushedValue::Histogram(sketch),
        };
        let lines = exp.format_metric(&metric);

        assert_eq!(lines.len(), 4); // count, avg, p50, p99
        assert!(lines[0].starts_with("latency.count:100|g"));
        assert!(lines[1].starts_with("latency.avg:"));
        assert!(lines[2].contains("latency.p50:"));
        assert!(lines[3].contains("latency.p99:"));
    }

    #[test]
    fn distribution_rejects_sketches_instead_of_exporting_the_mean() {
        let listener = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let exp = StatsdExporter::udp(listener.local_addr().unwrap().to_string())
            .histogram_mode(HistogramExportMode::Distribution)
            .build()
            .unwrap();
        let mut sketch = UDDSketch::new(200, 0.001);
        sketch.add_value(1.0);
        sketch.add_value(99.0);
        let result = exp.send_metrics(&[FlushedMetric {
            namespace: &[],
            metric_name: "latency",
            tags: make_tags(vec![]),
            value: FlushedValue::Histogram(sketch),
        }]);
        assert!(result.unwrap_err().message.contains("original samples"));
    }

    #[test]
    fn test_percentile_label() {
        assert_eq!(format_percentile_label(0.5), "p50");
        assert_eq!(format_percentile_label(0.9), "p90");
        assert_eq!(format_percentile_label(0.95), "p95");
        assert_eq!(format_percentile_label(0.99), "p99");
    }

    #[test]
    fn test_udp_exporter_creates() {
        // Just verify the builder compiles and creates a socket
        let _exp = StatsdExporter::udp("127.0.0.1:18125").build().unwrap();
    }

    #[test]
    fn test_send_raw_points_single_point() {
        let listener = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        listener
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let exp = StatsdExporter::udp(addr.to_string())
            .histogram_mode(HistogramExportMode::Distribution)
            .build()
            .unwrap();

        let points = vec![RawDistributionPoint {
            namespace: &[],
            metric_name: "latency",
            tags: make_tags(vec![]),
            value: 42.0,
        }];
        exp.send_raw_points(&points).unwrap();

        let mut buf = [0u8; 2048];
        let (n, _) = listener.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"latency:42|d");
    }

    #[test]
    fn test_send_raw_points_batches_into_multiple_datagrams() {
        let listener = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        listener
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let exp = StatsdExporter::udp(addr.to_string())
            .histogram_mode(HistogramExportMode::Distribution)
            .max_buffer_size(64)
            .build()
            .unwrap();

        let points: Vec<RawDistributionPoint> = (0..20)
            .map(|i| RawDistributionPoint {
                namespace: &[],
                metric_name: "some_long_latency_metric_name",
                tags: make_tags(vec![]),
                value: i as f64,
            })
            .collect();
        exp.send_raw_points(&points).unwrap();

        let mut datagram_count = 0;
        let mut buf = [0u8; 2048];
        while listener.recv_from(&mut buf).is_ok() {
            datagram_count += 1;
        }
        assert!(
            datagram_count > 1,
            "expected points to split across multiple datagrams, got {datagram_count}"
        );
    }
}
