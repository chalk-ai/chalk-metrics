use std::io;
use std::net::UdpSocket;
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::{ExportError, Exporter, FlushedMetric, FlushedValue};

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
    max_buffer_size: usize,
}

/// Builder for [`StatsdExporter`].
pub struct StatsdExporterBuilder {
    target: StatsdTarget,
    namespace: Option<String>,
    default_tags: Vec<(String, String)>,
    histogram_mode: HistogramExportMode,
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
        self
    }

    /// Set the maximum datagram buffer size in bytes (default: 1432).
    pub fn max_buffer_size(mut self, size: usize) -> Self {
        self.max_buffer_size = size;
        self
    }

    /// Build the exporter, creating the underlying socket.
    pub fn build(self) -> io::Result<StatsdExporter> {
        let socket = match self.target {
            StatsdTarget::Udp(addr) => {
                let sock = UdpSocket::bind("0.0.0.0:0")?;
                sock.connect(&addr)?;
                sock.set_nonblocking(true)?;
                Socket::Udp(sock)
            }
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
            max_buffer_size: self.max_buffer_size,
        })
    }
}

impl StatsdExporter {
    /// Create a builder targeting a UDP address (e.g., `"127.0.0.1:8125"`).
    pub fn udp(addr: impl Into<String>) -> StatsdExporterBuilder {
        StatsdExporterBuilder {
            target: StatsdTarget::Udp(addr.into()),
            namespace: None,
            default_tags: Vec::new(),
            histogram_mode: HistogramExportMode::default(),
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
        let mut buffer = String::with_capacity(self.max_buffer_size);

        for metric in metrics {
            let name = self.full_name(metric.namespace, metric.metric_name);
            let tags = self.format_tags(&metric.tags.pairs);

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
                        // Emit the mean as a distribution value
                        let mean = sketch.mean();
                        lines.push(format!("{name}:{mean}|d{tags}"));
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

            for line in lines {
                // If adding this line would exceed buffer size, flush first
                if !buffer.is_empty() && buffer.len() + 1 + line.len() > self.max_buffer_size {
                    self.send_buffer(&buffer)?;
                    buffer.clear();
                }
                if !buffer.is_empty() {
                    buffer.push('\n');
                }
                buffer.push_str(&line);
            }
        }

        if !buffer.is_empty() {
            self.send_buffer(&buffer)?;
        }

        Ok(())
    }

    fn send_buffer(&self, data: &str) -> Result<(), ExportError> {
        let bytes = data.as_bytes();
        match &self.socket {
            Socket::Udp(sock) => {
                // Non-blocking send, silently drop on failure
                let _ = sock.send(bytes);
            }
            #[cfg(unix)]
            Socket::Uds {
                socket,
                path,
                last_inode,
            } => {
                // Check for inode change (socket replacement)
                let current_inode = get_inode(path).unwrap_or(0);
                let stored_inode = last_inode.load(Ordering::Relaxed);
                if current_inode != stored_inode && current_inode != 0 {
                    // Socket was replaced, reconnect
                    last_inode.store(current_inode, Ordering::Relaxed);
                    let mut guard = socket.lock();
                    *guard = connect_uds(path).ok();
                }

                let guard = socket.lock();
                if let Some(ref sock) = *guard {
                    let _ = sock.send(bytes);
                }
            }
        }
        Ok(())
    }
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
    async fn export(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        self.send_metrics(metrics)
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
    fn test_format_histogram_distribution() {
        let exp = FormatOnlyExporter {
            namespace: None,
            default_tags: vec![],
            histogram_mode: HistogramExportMode::Distribution,
        };

        let mut sketch = UDDSketch::new(200, 0.001);
        sketch.add_value(42.0);

        let metric = FlushedMetric {
            namespace: &[],
            metric_name: "latency",
            tags: make_tags(vec![]),
            value: FlushedValue::Histogram(sketch),
        };
        let lines = exp.format_metric(&metric);
        assert_eq!(lines, vec!["latency:42|d"]);
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
}
