use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time;

use crate::aggregator::striped_map::StripedAggMap;
use crate::export::Exporter;

/// Spawns a dedicated OS thread running a tokio runtime for periodic
/// metric flushing and export.
///
/// Returns a handle to join the thread and a Notify for signaling shutdown.
pub(crate) fn spawn_flush_worker(
    aggregator: Arc<StripedAggMap>,
    exporters: Vec<Box<dyn Exporter>>,
    flush_interval: Duration,
    worker_threads: usize,
    shutdown: Arc<Notify>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("chalk-metrics-worker".into())
        .spawn(move || {
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
                let mut interval = time::interval(flush_interval);
                interval.tick().await; // first tick is immediate, skip it

                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            flush_and_export(&aggregator, &exporters).await;
                        }
                        _ = shutdown.notified() => {
                            // Final flush before shutdown
                            flush_and_export(&aggregator, &exporters).await;
                            break;
                        }
                    }
                }
            });
        })
        .expect("failed to spawn chalk-metrics worker thread")
}

async fn flush_and_export(aggregator: &StripedAggMap, exporters: &[Box<dyn Exporter>]) {
    let metrics = aggregator.flush();
    if metrics.is_empty() {
        return;
    }

    // Export to all exporters sequentially. For true concurrency with
    // slow exporters, users can increase worker_threads.
    for exporter in exporters.iter() {
        if let Err(e) = exporter.export(&metrics).await {
            eprintln!("chalk-metrics: export error: {e}");
        }
    }
}
