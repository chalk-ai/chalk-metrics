use std::borrow::Cow;
use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use super::{
    DistributionInput, ExportError, Exporter, FlushedMetric, FlushedValue, RawDistributionPoint,
};

/// How recorded histogram observations should be exported over StatsD.
#[derive(Debug, Clone)]
pub enum HistogramExportMode {
    /// Emit summary statistics as separate gauge metrics with a `.percentile` suffix.
    /// The `Vec<f64>` contains the percentiles to emit (e.g., `[0.5, 0.9, 0.95, 0.99]`).
    /// Also emits `.count` and `.avg`.
    Percentiles(Vec<f64>),
    /// Emit original observations as a DogStatsD distribution (`|d`). Only works
    /// with a Datadog agent. MetricsClient routes raw samples automatically;
    /// direct callers must use `Exporter::export_raw_points`, not sketches.
    Distribution,
}

impl Default for HistogramExportMode {
    fn default() -> Self {
        HistogramExportMode::Percentiles(vec![0.5, 0.9, 0.95, 0.99])
    }
}

/// Totals of data a [`StatsdExporter`] could not send and discarded.
///
/// The exporter never waits on the socket: a datagram the socket does not
/// accept (a full receiver queue, a missing receiver, any send error) is
/// dropped and counted here. A single line longer than `max_buffer_size` is
/// also dropped and counted in `lines` and `bytes` (not `datagrams`). Obtain a
/// handle with [`StatsdExporter::drop_counters`] before handing the exporter
/// to the client.
#[derive(Debug, Default)]
pub struct StatsdDropCounters {
    datagrams: AtomicU64,
    lines: AtomicU64,
    bytes: AtomicU64,
}

impl StatsdDropCounters {
    /// Datagrams dropped because sending them failed.
    pub fn datagrams(&self) -> u64 {
        self.datagrams.load(Ordering::Relaxed)
    }

    /// Metric lines dropped, in failed datagrams or for exceeding the size limit.
    pub fn lines(&self) -> u64 {
        self.lines.load(Ordering::Relaxed)
    }

    /// Bytes of dropped lines and datagrams (line bytes only for oversized lines).
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
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
    drops: Arc<StatsdDropCounters>,
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
            drops: Arc::new(StatsdDropCounters::default()),
        })
    }
}

impl StatsdExporter {
    /// Create a builder targeting an IPv4/IPv6 address or hostname with a port
    /// (e.g., `"127.0.0.1:8125"`, `"[::1]:8125"`, or `"localhost:8125"`).
    /// Dual-stack hostnames prefer IPv4 for compatibility with IPv4-only agents.
    /// IPv6 literals and IPv6-only hostnames use IPv6. UDP connect cannot verify
    /// that an agent is listening; successful sends are not delivery acknowledgments.
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

    /// Shared counters of datagrams and lines dropped because the socket did
    /// not accept them. Call before handing the exporter to the client.
    pub fn drop_counters(&self) -> Arc<StatsdDropCounters> {
        Arc::clone(&self.drops)
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
    /// Lines are produced lazily, so datagrams go out while later metrics are
    /// still being formatted.
    fn send_metrics(&self, metrics: &[FlushedMetric]) -> Result<(), ExportError> {
        let mut rejected = None;
        let lines = metrics.iter().flat_map(|metric| {
            let name = self.full_name(metric.namespace, metric.metric_name);
            let tags = self.format_tags(&metric.tags.pairs);

            match &metric.value {
                FlushedValue::Count(value) => Lines::One(Some(format!("{name}:{value}|c{tags}"))),
                FlushedValue::Gauge(value) => Lines::One(Some(format!("{name}:{value}|g{tags}"))),
                FlushedValue::Histogram(sketch) => match &self.histogram_mode {
                    HistogramExportMode::Distribution => {
                        rejected.get_or_insert_with(|| ExportError::new(
                            "Distribution requires original samples; use MetricsClient or export_raw_points",
                        ));
                        Lines::One(None)
                    }
                    HistogramExportMode::Percentiles(percentiles) => {
                        let count = sketch.count();
                        let avg = sketch.mean();
                        let mut lines = Vec::with_capacity(2 + percentiles.len());
                        lines.push(format!("{name}.count:{count}|g{tags}"));
                        lines.push(format!("{name}.avg:{avg}|g{tags}"));
                        for &p in percentiles {
                            let value = sketch.estimate_quantile(p);
                            let label = format_percentile_label(p);
                            lines.push(format!("{name}.{label}:{value}|g{tags}"));
                        }
                        Lines::Many(lines.into_iter())
                    }
                },
            }
        });

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

    fn pack_distribution_points<'a>(
        &'a self,
        points: &'a [RawDistributionPoint],
    ) -> impl Iterator<Item = String> + 'a {
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

        // Format each group's lines only when the sender asks for them.
        groups.into_iter().flat_map(move |(point, values)| {
            let name = self.full_name(point.namespace, point.metric_name);
            let tags = self.format_tags(&point.tags.pairs);
            if values.is_empty() {
                return Lines::One(Some(format_distribution_line(&name, point.value, &tags)));
            }
            let prefix = format!("{name}:");
            let suffix = format!("|d{tags}");
            let mut lines = Vec::new();
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
            Lines::Many(lines.into_iter())
        })
    }

    fn send_lines(&self, lines: impl IntoIterator<Item = String>) -> Result<(), ExportError> {
        self.send_lines_with(lines, |data| self.send_buffer(data))
    }

    fn send_lines_with(
        &self,
        lines: impl IntoIterator<Item = String>,
        mut send: impl FnMut(&str) -> Result<(), ExportError>,
    ) -> Result<(), ExportError> {
        let mut buffer = self.buffer.lock();
        buffer.clear();

        let mut rejected = None;
        let mut buffered_lines = 0u64;
        for line in lines {
            if line.len() > self.max_buffer_size {
                self.drops.lines.fetch_add(1, Ordering::Relaxed);
                self.drops
                    .bytes
                    .fetch_add(line.len() as u64, Ordering::Relaxed);
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
                if let Err(error) = send(&buffer) {
                    self.count_dropped_datagram(buffered_lines, buffer.len());
                    rejected.get_or_insert(error);
                }
                buffer.clear();
                buffered_lines = 0;
            }
            if !buffer.is_empty() {
                buffer.push('\n');
            }
            buffer.push_str(&line);
            buffered_lines += 1;
        }

        if !buffer.is_empty()
            && let Err(error) = send(&buffer)
        {
            self.count_dropped_datagram(buffered_lines, buffer.len());
            rejected.get_or_insert(error);
        }

        match rejected {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn count_dropped_datagram(&self, lines: u64, bytes: usize) {
        self.drops.datagrams.fetch_add(1, Ordering::Relaxed);
        self.drops.lines.fetch_add(lines, Ordering::Relaxed);
        self.drops.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
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
                // A full receiver queue does not invalidate this connection.
                // Reconnecting for each later datagram would amplify backpressure.
                if let Err(error) = &result
                    && error.kind() != io::ErrorKind::WouldBlock
                {
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
    connect_udp_addresses(destination.to_socket_addrs()?)
}

fn connect_udp_addresses(addresses: impl IntoIterator<Item = SocketAddr>) -> io::Result<UdpSocket> {
    // UDP connect is local setup, not a probe for a listening receiver. Preserve
    // IPv4 hostname behavior while allowing explicit or IPv6-only destinations.
    let mut addresses: Vec<_> = addresses.into_iter().collect();
    addresses.sort_by_key(|address| address.is_ipv6());
    let mut last_error = io::Error::new(
        io::ErrorKind::AddrNotAvailable,
        "UDP destination resolved to no addresses",
    );
    for address in addresses {
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

/// Lines produced for one metric or series: a single line or a short batch.
enum Lines {
    One(Option<String>),
    Many(std::vec::IntoIter<String>),
}

impl Iterator for Lines {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        match self {
            Lines::One(line) => line.take(),
            Lines::Many(lines) => lines.next(),
        }
    }
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

    #[test]
    fn udp_prefers_ipv4_even_when_ipv6_is_resolved_first() {
        let ipv4 = UdpSocket::bind("127.0.0.1:0").unwrap();
        let first = ipv4.local_addr().unwrap();
        let ipv6 = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], first.port()));
        let later_ipv4 = SocketAddr::from(([127, 0, 0, 2], first.port()));
        let socket = connect_udp_addresses([ipv6, first, later_ipv4]).unwrap();
        assert_eq!(socket.peer_addr().unwrap(), first);
        socket.send(b"latency:1:99|d").unwrap();
        ipv4.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut buffer = [0; 64];
        let n = ipv4.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"latency:1:99|d");
    }

    #[test]
    fn udp_supports_ipv6_only_resolution_and_empty_resolution_errors() {
        let ipv6 = UdpSocket::bind("[::1]:0").unwrap();
        let address = ipv6.local_addr().unwrap();
        assert_eq!(
            connect_udp_addresses([address])
                .unwrap()
                .peer_addr()
                .unwrap(),
            address
        );
        assert_eq!(
            connect_udp_addresses([]).unwrap_err().kind(),
            io::ErrorKind::AddrNotAvailable
        );
    }

    #[cfg(unix)]
    #[test]
    fn uds_backpressure_retains_connection_and_recovers_after_drain() {
        use std::os::fd::AsRawFd;
        let path = std::env::temp_dir().join(format!(
            "chalk-metrics-{}-backpressure.sock",
            std::process::id()
        ));
        let listener = UnixDatagram::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let exporter = StatsdExporter::uds(&path).build().unwrap();
        let socket_fd = || {
            let Socket::Uds { socket, .. } = &exporter.socket else {
                unreachable!()
            };
            socket
                .lock()
                .as_ref()
                .expect("connection must be retained")
                .as_raw_fd()
        };
        let original_fd = socket_fd();
        let mut blocked = false;
        for _ in 0..4096 {
            if let Err(error) = exporter.send_buffer("x:1|d") {
                assert_eq!(
                    error
                        .source
                        .unwrap()
                        .downcast_ref::<io::Error>()
                        .unwrap()
                        .kind(),
                    io::ErrorKind::WouldBlock
                );
                blocked = true;
                break;
            }
        }
        assert!(blocked, "receiver must actually exert backpressure");
        for _ in 0..3 {
            assert!(exporter.send_buffer("x:2|d").is_err());
            assert_eq!(socket_fd(), original_fd);
        }
        let mut buffer = [0; 64];
        while listener.recv(&mut buffer).is_ok() {}
        exporter.send_buffer("x:99|d").unwrap();
        assert_eq!(socket_fd(), original_fd);
        let n = listener.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"x:99|d");
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn uds_permanent_send_error_clears_connection_and_reconnects() {
        let path = std::env::temp_dir().join(format!(
            "chalk-metrics-{}-reconnect.sock",
            std::process::id()
        ));
        let listener = UnixDatagram::bind(&path).unwrap();
        let exporter = StatsdExporter::uds(&path).build().unwrap();
        drop(listener);
        std::fs::remove_file(&path).unwrap();
        assert!(exporter.send_buffer("x:1|d").is_err());
        let Socket::Uds { socket, .. } = &exporter.socket else {
            unreachable!()
        };
        assert!(socket.lock().is_none());
        let replacement = UnixDatagram::bind(&path).unwrap();
        replacement
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        exporter.send_buffer("x:99|d").unwrap();
        let mut buffer = [0; 64];
        let n = replacement.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"x:99|d");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_datagram_does_not_skip_later_packets_or_replay_successes() {
        let exporter = StatsdExporter::udp("127.0.0.1:8125")
            .max_buffer_size(5)
            .build()
            .unwrap();
        let mut attempted = Vec::new();
        let result = exporter.send_lines_with(
            ["x:1|d".to_owned(), "x:2|d".to_owned(), "x:3|d".to_owned()],
            |packet| {
                attempted.push(packet.to_owned());
                if attempted.len() == 2 {
                    Err(ExportError::new("injected send failure"))
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result.unwrap_err().message, "injected send failure");
        assert_eq!(attempted, ["x:1|d", "x:2|d", "x:3|d"]);
    }
    #[test]
    fn failed_sends_and_oversized_lines_are_counted() {
        let exporter = StatsdExporter::udp("127.0.0.1:8125")
            .max_buffer_size(11)
            .build()
            .unwrap();
        let counters = exporter.drop_counters();
        let mut attempt = 0;
        let result = exporter.send_lines_with(
            [
                "x:1|d".to_owned(),
                "y:2|d".to_owned(),
                "this-line-is-too-long".to_owned(),
                "z:3|d".to_owned(),
                "w:4|d".to_owned(),
            ],
            |_| {
                attempt += 1;
                if attempt == 1 {
                    Err(ExportError::new("injected send failure"))
                } else {
                    Ok(())
                }
            },
        );
        // The oversized line was rejected first, so its error takes precedence.
        assert!(result.unwrap_err().message.contains("21 bytes"));
        // First datagram "x:1|d\ny:2|d" (2 lines, 11 bytes) failed; the oversized
        // line (21 bytes) counts as a line and bytes only; the second datagram
        // "z:3|d\nw:4|d" was sent.
        assert_eq!(counters.datagrams(), 1);
        assert_eq!(counters.lines(), 3);
        assert_eq!(counters.bytes(), 11 + 21);
    }

    use crate::aggregator::sketch::UDDSketch;
    use crate::aggregator::striped_map::TagsData;

    fn make_tags(pairs: Vec<(&'static str, std::borrow::Cow<'static, str>)>) -> Arc<TagsData> {
        Arc::new(TagsData { pairs })
    }

    #[test]
    fn production_packets_format_namespaces_tags_and_all_metric_types() {
        let listener = UdpSocket::bind("127.0.0.1:0").unwrap();
        listener
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let exporter = StatsdExporter::udp(listener.local_addr().unwrap().to_string())
            .namespace("app")
            .default_tag("env", "prod")
            .histogram_mode(HistogramExportMode::Percentiles(vec![0.5, 0.99]))
            .build()
            .unwrap();
        let mut sketch = UDDSketch::new(200, 0.001);
        for value in 1..=100 {
            sketch.add_value(f64::from(value));
        }
        let tags = make_tags(vec![("status", "ok".into())]);
        let metrics = [
            FlushedMetric {
                namespace: &["http"],
                metric_name: "requests",
                tags: Arc::clone(&tags),
                value: FlushedValue::Count(7),
            },
            FlushedMetric {
                namespace: &["http"],
                metric_name: "active",
                tags: Arc::clone(&tags),
                value: FlushedValue::Gauge(4.0),
            },
            FlushedMetric {
                namespace: &["http"],
                metric_name: "latency",
                tags,
                value: FlushedValue::Histogram(sketch),
            },
        ];
        exporter.send_metrics(&metrics).unwrap();
        let mut buffer = [0; 4096];
        let n = listener.recv(&mut buffer).unwrap();
        let lines: Vec<_> = std::str::from_utf8(&buffer[..n]).unwrap().lines().collect();
        assert_eq!(lines.len(), 6);
        assert!(lines.contains(&"app.http.requests:7|c|#env:prod,status:ok"));
        assert!(lines.contains(&"app.http.active:4|g|#env:prod,status:ok"));
        assert!(lines.contains(&"app.http.latency.count:100|g|#env:prod,status:ok"));
        assert!(lines.contains(&"app.http.latency.avg:50.5|g|#env:prod,status:ok"));
        for (label, expected) in [("p50", 50.0), ("p99", 99.0)] {
            let prefix = format!("app.http.latency.{label}:");
            let line = lines.iter().find(|line| line.starts_with(&prefix)).unwrap();
            let value: f64 = line[prefix.len()..]
                .split('|')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            assert!((value - expected).abs() < 2.0);
        }
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
