use std::sync::Arc;
use std::time::Duration;

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
            // build enables tokio's `net` feature (e.g. a custom exporter).
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

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            flush_and_export(&aggregator, &exporters).await;
                        }
                        _ = raw_interval.tick(), if raw_receiver.is_some() => {
                            drain_raw_and_export(&mut raw_receiver, &exporters).await;
                        }
                        _ = shutdown.notified() => {
                            // Prevent new raw admissions, then drain everything accepted.
                            if let Some(receiver) = &mut raw_receiver { receiver.close(); }
                            flush_and_export(&aggregator, &exporters).await;
                            drain_raw_and_export(&mut raw_receiver, &exporters).await;
                            break;
                        }
                    }
                }
            });
        })
        .expect("failed to spawn chalk-metrics worker thread")
}

async fn flush_and_export(aggregator: &StripedAggMap, exporters: &[ConfiguredExporter]) {
    let metrics = aggregator.flush();
    if metrics.is_empty() {
        return;
    }
    for configured in exporters {
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
            eprintln!("chalk-metrics: export error: {e}");
        }
    }
}

async fn drain_raw_and_export(
    raw_receiver: &mut Option<mpsc::Receiver<RawDistributionPoint>>,
    exporters: &[ConfiguredExporter],
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

    for configured in exporters.iter().filter(|e| e.input.wants_raw()) {
        if let Err(e) = configured.exporter.export_raw_points(&points).await {
            eprintln!("chalk-metrics: export_raw_points error: {e}");
        }
    }
}
