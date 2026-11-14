//! Concurrent recording against concurrent flushes, with idle-series retention
//! off and on: nothing may be lost, duplicated or emitted without a record.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use chalk_metrics::client::{self, MetricsClient};
use chalk_metrics::export::FlushedValue;

const SERIES: usize = 48;
const THREADS: usize = 8;
const OPS_PER_THREAD: usize = 20_000;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// Slots 0..SERIES are count series, SERIES..2*SERIES are histogram series.
#[derive(Default, Clone, PartialEq, Debug)]
struct Totals {
    count: i64,
    hist_n: u64,
    hist_sum: f64,
}

fn tags(series: usize) -> Vec<(&'static str, std::borrow::Cow<'static, str>)> {
    vec![("series", series.to_string().into())]
}

fn run(retain: u32, seed: u64) {
    let client = client::builder()
        .flush_interval(Duration::from_secs(3600))
        .retain_idle_series(retain)
        .build_local();
    let started: Vec<AtomicU64> = (0..2 * SERIES).map(|_| AtomicU64::new(0)).collect();
    let completed: Vec<AtomicU64> = (0..2 * SERIES).map(|_| AtomicU64::new(0)).collect();
    let done = AtomicBool::new(false);
    let mut expected = vec![Totals::default(); 2 * SERIES];
    let mut flushed_totals = vec![Totals::default(); 2 * SERIES];

    std::thread::scope(|s| {
        let (client, started, completed, done) = (&client, &started, &completed, &done);
        let recorders: Vec<_> = (0..THREADS)
            .map(|t| {
                s.spawn(move || {
                    let mut rng = Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) ^ (t as u64 + 1));
                    let mut mine = vec![Totals::default(); 2 * SERIES];
                    for op in 0..OPS_PER_THREAD {
                        let r = rng.next();
                        let series = (r >> 8) as usize % SERIES;
                        if r & 1 == 0 {
                            let delta = ((r >> 32) % 9) as i64 - 3;
                            started[series].fetch_add(1, Ordering::SeqCst);
                            client.record_count(
                                "stress_c",
                                &[],
                                series as u64,
                                || tags(series),
                                delta,
                            );
                            completed[series].fetch_add(1, Ordering::SeqCst);
                            mine[series].count += delta;
                        } else {
                            let value = 1 + (r >> 32) % 100;
                            let slot = SERIES + series;
                            started[slot].fetch_add(1, Ordering::SeqCst);
                            client.record_histogram(
                                "stress_h",
                                &[],
                                series as u64,
                                || tags(series),
                                value as f64,
                            );
                            completed[slot].fetch_add(1, Ordering::SeqCst);
                            mine[slot].hist_n += 1;
                            mine[slot].hist_sum += value as f64;
                        }
                        // Leave gaps so that some series sit idle between flushes.
                        if op % 512 == 0 {
                            std::thread::yield_now();
                        }
                    }
                    mine
                })
            })
            .collect();

        let flusher = s.spawn(move || {
            let mut totals = vec![Totals::default(); 2 * SERIES];
            let mut previous_before: Vec<u64> = vec![0; 2 * SERIES];
            let mut absorb = |client: &MetricsClient| {
                let before: Vec<u64> = completed.iter().map(|c| c.load(Ordering::SeqCst)).collect();
                let flushed = client.flush();
                let after: Vec<u64> = started.iter().map(|c| c.load(Ordering::SeqCst)).collect();
                let mut seen = HashSet::new();
                for metric in flushed {
                    let series: usize = metric.tags.pairs[0].1.parse().unwrap();
                    let slot = match metric.value {
                        FlushedValue::Count(_) => series,
                        FlushedValue::Histogram(_) => SERIES + series,
                        FlushedValue::Gauge(_) => unreachable!(),
                    };
                    assert!(
                        seen.insert(slot),
                        "series {slot} appears twice in one flush"
                    );
                    // A record emitted now began after the previous flush took
                    // its snapshot of completed records, and before this flush
                    // ended, so the started counter must have moved past it.
                    assert!(
                        after[slot] > previous_before[slot],
                        "series {slot} emitted without a record since the last flush"
                    );
                    match metric.value {
                        FlushedValue::Count(v) => totals[slot].count += v,
                        FlushedValue::Histogram(sketch) => {
                            assert!(sketch.count() > 0);
                            totals[slot].hist_n += sketch.count();
                            totals[slot].hist_sum += sketch.sum();
                        }
                        FlushedValue::Gauge(_) => unreachable!(),
                    }
                }
                previous_before = before;
            };
            loop {
                absorb(client);
                if done.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
            // Flushes after the recorders stopped: nothing was recorded since
            // the drain above, so nothing may be emitted.
            for _ in 0..3 {
                absorb(client);
            }
            totals
        });

        for recorder in recorders {
            for (slot, mine) in recorder.join().unwrap().into_iter().enumerate() {
                expected[slot].count += mine.count;
                expected[slot].hist_n += mine.hist_n;
                expected[slot].hist_sum += mine.hist_sum;
            }
        }
        done.store(true, Ordering::SeqCst);
        flushed_totals = flusher.join().unwrap();
    });

    for slot in 0..2 * SERIES {
        assert_eq!(
            expected[slot], flushed_totals[slot],
            "retain={retain} seed={seed} slot={slot}"
        );
    }
    client.shutdown();
}

#[test]
fn concurrent_flush_conserves_records_without_retention() {
    for seed in 1..=4 {
        run(0, seed);
    }
}

#[test]
fn concurrent_flush_conserves_records_with_retention() {
    for seed in 1..=4 {
        run(2, seed);
    }
}
