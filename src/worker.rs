use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc};
use tokio::time;

use crate::aggregator::striped_map::StripedAggMap;
use crate::export::{ConfiguredExporter, FlushedValue, RawDistributionPoint};

/// Spawns a dedicated OS thread running a tokio runtime for periodic
/// metric flushing and export.
///
/// Returns a handle to join the thread and a Notify for signaling shutdown.
pub(crate) fn spawn_flush_worker(
    aggregator: Arc<StripedAggMap>,
    exporters: Vec<ConfiguredExporter>,
    flush_interval: Duration,
    raw_sample_flush_interval: Duration,
    worker_threads: usize,
    shutdown: Arc<Notify>,
    raw_receiver: Option<mpsc::Receiver<RawDistributionPoint>>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("chalk-metrics-worker".into())
        .spawn(move || {
            // `enable_all` turns on the I/O driver only when some crate in the
            // build enables a tokio feature that needs it, such as `net` for a
            // custom exporter.
            let rt = if worker_threads <= 1 {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime for chalk-metrics")
            } else {
                tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(worker_threads)
                    .enable_all()
                    .build()
                    .expect("failed to create tokio runtime for chalk-metrics")
            };

            rt.block_on(async move {
                let exporters = Arc::new(exporters);
                let mut raw_receiver = raw_receiver;
                let mut interval = time::interval(flush_interval);
                interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                interval.tick().await; // first tick is immediate, skip it
                let mut raw_interval = time::interval(raw_sample_flush_interval);
                raw_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                raw_interval.tick().await;
                // Early drain: check ten times per raw interval and export
                // only once the queue is a quarter full, so bursts stay small.
                let mut early_interval =
                    time::interval((raw_sample_flush_interval / 10).max(Duration::from_millis(1)));
                early_interval.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
                early_interval.tick().await;
                // One limiter per exporter and kind, so one exporter's errors
                // never hide another's.
                let mut aggregate_errors: Vec<ErrorLimiter> =
                    exporters.iter().map(|_| ErrorLimiter::default()).collect();
                let mut raw_errors: Vec<ErrorLimiter> =
                    exporters.iter().map(|_| ErrorLimiter::default()).collect();

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            flush_and_export(&aggregator, &exporters, &mut aggregate_errors).await;
                        }
                        _ = raw_interval.tick(), if raw_receiver.is_some() => {
                            drain_raw_and_export(&mut raw_receiver, &exporters, &mut raw_errors).await;
                        }
                        _ = early_interval.tick(), if raw_receiver.is_some() => {
                            if raw_queue_is_quarter_full(&raw_receiver) {
                                drain_raw_and_export(&mut raw_receiver, &exporters, &mut raw_errors).await;
                                // Count the next regular flush from now, so it does
                                // not follow at once with a small leftover batch.
                                raw_interval.reset();
                            }
                        }
                        _ = shutdown.notified() => {
                            // Prevent new raw admissions, then drain everything accepted.
                            if let Some(receiver) = &mut raw_receiver { receiver.close(); }
                            flush_and_export(&aggregator, &exporters, &mut aggregate_errors).await;
                            drain_raw_and_export(&mut raw_receiver, &exporters, &mut raw_errors).await;
                            // Nothing prints after this: report what was held back.
                            for limiter in &mut aggregate_errors {
                                limiter.finish("export");
                            }
                            for limiter in &mut raw_errors {
                                limiter.finish("export_raw_points");
                            }
                            break;
                        }
                    }
                }
            });
        })
        .expect("failed to spawn chalk-metrics worker thread")
}

/// Prints at most one error line per second, noting how many were suppressed.
#[derive(Default)]
struct ErrorLimiter {
    last_print: Option<Instant>,
    suppressed: u64,
    last_suppressed: Option<String>,
}

impl ErrorLimiter {
    fn report(&mut self, kind: &str, error: &dyn std::fmt::Display) {
        let now = Instant::now();
        if self
            .last_print
            .is_some_and(|last| now.duration_since(last) < Duration::from_secs(1))
        {
            self.suppressed += 1;
            self.last_suppressed = Some(error.to_string());
            return;
        }
        if self.suppressed > 0 {
            eprintln!(
                "chalk-metrics: {kind} error: {error} ({} similar errors suppressed)",
                self.suppressed
            );
        } else {
            eprintln!("chalk-metrics: {kind} error: {error}");
        }
        self.last_print = Some(now);
        self.suppressed = 0;
        self.last_suppressed = None;
    }

    /// Prints the most recent suppressed error, if any, with the count held back.
    fn finish(&mut self, kind: &str) {
        if let Some(error) = self.last_suppressed.take() {
            eprintln!(
                "chalk-metrics: {kind} error: {error} ({} errors since the last report)",
                self.suppressed
            );
            self.suppressed = 0;
        }
    }
}

fn raw_queue_is_quarter_full(raw_receiver: &Option<mpsc::Receiver<RawDistributionPoint>>) -> bool {
    raw_receiver
        .as_ref()
        .is_some_and(|receiver| receiver.len() >= (receiver.max_capacity() / 4).max(1))
}

async fn flush_and_export(
    aggregator: &StripedAggMap,
    exporters: &[ConfiguredExporter],
    errors: &mut [ErrorLimiter],
) {
    let metrics = aggregator.flush();
    if metrics.is_empty() {
        return;
    }
    for (configured, errors) in exporters.iter().zip(errors) {
        // Avoid cloning sketches. Only raw-only consumers need a filtered batch.
        let filtered;
        let batch = if configured.input.wants_sketch() {
            &metrics[..]
        } else {
            filtered = metrics
                .iter()
                .filter_map(|metric| {
                    let value = match metric.value {
                        FlushedValue::Count(value) => FlushedValue::Count(value),
                        FlushedValue::Gauge(value) => FlushedValue::Gauge(value),
                        FlushedValue::Histogram(_) => return None,
                    };
                    Some(crate::export::FlushedMetric {
                        namespace: metric.namespace,
                        metric_name: metric.metric_name,
                        tags: Arc::clone(&metric.tags),
                        value,
                    })
                })
                .collect::<Vec<_>>();
            &filtered[..]
        };
        if !batch.is_empty()
            && let Err(e) = configured.exporter.export(batch).await
        {
            errors.report("export", &e);
        }
    }
}

async fn drain_raw_and_export(
    raw_receiver: &mut Option<mpsc::Receiver<RawDistributionPoint>>,
    exporters: &[ConfiguredExporter],
    errors: &mut [ErrorLimiter],
) {
    let Some(receiver) = raw_receiver else {
        return;
    };

    // Snapshot the batch size: continuous producers cannot make this drain
    // unbounded or starve shutdown. A closed queue's snapshot is the final batch.
    let batch_size = receiver.len();
    let mut points = Vec::with_capacity(batch_size);
    for _ in 0..batch_size {
        match receiver.try_recv() {
            Ok(point) => points.push(point),
            Err(_) => break,
        }
    }
    if points.is_empty() {
        return;
    }

    for (configured, errors) in exporters
        .iter()
        .zip(errors)
        .filter(|(configured, _)| configured.input.wants_raw())
    {
        if let Err(e) = configured.exporter.export_raw_points(&points).await {
            errors.report("export_raw_points", &e);
        }
    }
}
