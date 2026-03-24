use std::sync::Arc;

use hashbrown::HashMap;
use parking_lot::Mutex;

use super::count::CountSlot;
use super::gauge::GaugeSlot;
use super::histogram::HistogramSlot;
use crate::export::{FlushedMetric, FlushedValue};

const STRIPE_COUNT: usize = 64;
const STRIPE_MASK: usize = 63;

/// Tag data stored in the aggregation map, used by exporters during flush.
/// Contains `(export_name, value)` pairs computed once on insertion.
#[derive(Debug, Clone)]
pub struct TagsData {
    pub pairs: Vec<(&'static str, String)>,
}

/// Key stored in the aggregation map.
#[derive(Debug)]
struct AggKey {
    namespace: &'static [&'static str],
    metric_name: &'static str,
    tags_data: Arc<TagsData>,
}

/// Aggregation slot: one of count, gauge, or histogram.
enum AggSlot {
    Count(CountSlot),
    Gauge(GaugeSlot),
    Histogram(HistogramSlot),
}

type Stripe = Mutex<HashMap<u64, (AggKey, AggSlot)>>;

pub struct StripedAggMap {
    stripes: Box<[Stripe]>,
    max_buckets: u64,
    initial_error: f64,
}

impl StripedAggMap {
    pub fn new(max_buckets: u64, initial_error: f64) -> Self {
        let stripes: Vec<_> = (0..STRIPE_COUNT)
            .map(|_| Mutex::new(HashMap::new()))
            .collect();
        Self {
            stripes: stripes.into_boxed_slice(),
            max_buckets,
            initial_error,
        }
    }

    #[inline]
    pub fn record_count(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, String)>,
        delta: i64,
    ) {
        let combined = combine_hash(metric_name, tags_hash);
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();

        let entry = guard
            .raw_entry_mut()
            .from_hash(combined, |k| *k == combined);

        match entry {
            hashbrown::hash_map::RawEntryMut::Occupied(e) => {
                if let AggSlot::Count(ref slot) = e.get().1 {
                    slot.record(delta);
                }
            }
            hashbrown::hash_map::RawEntryMut::Vacant(e) => {
                let slot = CountSlot::new();
                slot.record(delta);
                let key = AggKey {
                    namespace,
                    metric_name,
                    tags_data: Arc::new(TagsData { pairs: make_tags() }),
                };
                e.insert_hashed_nocheck(combined, combined, (key, AggSlot::Count(slot)));
            }
        }
    }

    #[inline]
    pub fn record_gauge(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, String)>,
        value: f64,
    ) {
        let combined = combine_hash(metric_name, tags_hash);
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();

        let entry = guard
            .raw_entry_mut()
            .from_hash(combined, |k| *k == combined);

        match entry {
            hashbrown::hash_map::RawEntryMut::Occupied(e) => {
                if let AggSlot::Gauge(ref slot) = e.get().1 {
                    slot.record(value);
                }
            }
            hashbrown::hash_map::RawEntryMut::Vacant(e) => {
                let slot = GaugeSlot::new();
                slot.record(value);
                let key = AggKey {
                    namespace,
                    metric_name,
                    tags_data: Arc::new(TagsData { pairs: make_tags() }),
                };
                e.insert_hashed_nocheck(combined, combined, (key, AggSlot::Gauge(slot)));
            }
        }
    }

    #[inline]
    pub fn record_histogram(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, String)>,
        value: f64,
    ) {
        let combined = combine_hash(metric_name, tags_hash);
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();

        let entry = guard
            .raw_entry_mut()
            .from_hash(combined, |k| *k == combined);

        match entry {
            hashbrown::hash_map::RawEntryMut::Occupied(e) => {
                if let AggSlot::Histogram(ref slot) = e.get().1 {
                    slot.record(value);
                }
            }
            hashbrown::hash_map::RawEntryMut::Vacant(e) => {
                let slot = HistogramSlot::new(self.max_buckets, self.initial_error);
                slot.record(value);
                let key = AggKey {
                    namespace,
                    metric_name,
                    tags_data: Arc::new(TagsData { pairs: make_tags() }),
                };
                e.insert_hashed_nocheck(combined, combined, (key, AggSlot::Histogram(slot)));
            }
        }
    }

    pub fn flush(&self) -> Vec<FlushedMetric> {
        let mut flushed = Vec::new();

        for stripe in self.stripes.iter() {
            let mut guard = stripe.lock();
            guard.retain(|_combined, (key, slot)| match slot {
                AggSlot::Count(count_slot) => {
                    let value = count_slot.flush();
                    flushed.push(FlushedMetric {
                        namespace: key.namespace,
                        metric_name: key.metric_name,
                        tags: Arc::clone(&key.tags_data),
                        value: FlushedValue::Count(value),
                    });
                    false
                }
                AggSlot::Gauge(gauge_slot) => {
                    if let Some(value) = gauge_slot.flush() {
                        flushed.push(FlushedMetric {
                            namespace: key.namespace,
                            metric_name: key.metric_name,
                            tags: Arc::clone(&key.tags_data),
                            value: FlushedValue::Gauge(value),
                        });
                    }
                    true
                }
                AggSlot::Histogram(hist_slot) => {
                    let sketch = hist_slot.flush();
                    if sketch.count() > 0 {
                        flushed.push(FlushedMetric {
                            namespace: key.namespace,
                            metric_name: key.metric_name,
                            tags: Arc::clone(&key.tags_data),
                            value: FlushedValue::Histogram(sketch),
                        });
                    }
                    false
                }
            });
        }

        flushed
    }
}

/// Use the static string pointer as part of the hash key. Since metric names
/// are `&'static str` from generated code, identical strings share the same
/// address, making this an O(1) identity check.
#[inline]
fn combine_hash(metric_name: &'static str, tags_hash: u64) -> u64 {
    let name_hash = metric_name.as_ptr() as u64;
    tags_hash
        .wrapping_mul(6364136223846793005)
        .wrapping_add(name_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_map() -> StripedAggMap {
        StripedAggMap::new(200, 0.001)
    }

    #[test]
    fn test_insert_and_record_count() {
        let map = make_map();
        map.record_count("test_count", &[], 123, || vec![("k", "v".into())], 5);
        map.record_count("test_count", &[], 123, || panic!("should not call"), 3);

        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        match &flushed[0].value {
            FlushedValue::Count(v) => assert_eq!(*v, 8),
            _ => panic!("expected count"),
        }
        assert_eq!(flushed[0].metric_name, "test_count");
        assert!(flushed[0].namespace.is_empty());
    }

    #[test]
    fn test_insert_with_namespace() {
        let map = make_map();
        map.record_count("request_count", &["http"], 100, || vec![], 1);

        let flushed = map.flush();
        assert_eq!(flushed[0].namespace, &["http"]);
        assert_eq!(flushed[0].metric_name, "request_count");
    }

    #[test]
    fn test_insert_and_record_gauge() {
        let map = make_map();
        map.record_gauge("test_gauge", &[], 456, || vec![], 10.0);
        map.record_gauge("test_gauge", &[], 456, || panic!("should not call"), 20.0);

        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        match &flushed[0].value {
            FlushedValue::Gauge(v) => assert_eq!(*v, 20.0),
            _ => panic!("expected gauge"),
        }
    }

    #[test]
    fn test_insert_and_record_histogram() {
        let map = make_map();
        map.record_histogram("test_hist", &[], 789, || vec![], 42.0);
        map.record_histogram("test_hist", &[], 789, || panic!("should not call"), 43.0);

        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        match &flushed[0].value {
            FlushedValue::Histogram(sketch) => assert_eq!(sketch.count(), 2),
            _ => panic!("expected histogram"),
        }
    }

    #[test]
    fn test_different_metrics() {
        let map = make_map();
        map.record_count("count_a", &[], 100, || vec![], 1);
        map.record_count("count_b", &[], 100, || vec![], 2);
        assert_eq!(map.flush().len(), 2);
    }

    #[test]
    fn test_flush_removes_count_entries() {
        let map = make_map();
        map.record_count("c", &[], 100, || vec![], 5);
        assert_eq!(map.flush().len(), 1);
        assert_eq!(map.flush().len(), 0);
    }

    #[test]
    fn test_flush_retains_gauge_entries() {
        let map = make_map();
        map.record_gauge("g", &[], 100, || vec![], 42.0);
        assert_eq!(map.flush().len(), 1);
        assert_eq!(map.flush().len(), 1);
    }

    #[test]
    fn test_concurrent_same_key() {
        use std::sync::Arc;
        use std::thread;

        let map = Arc::new(make_map());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    for _ in 0..1000 {
                        map.record_count("c", &[], 42, || vec![("t", "v".into())], 1);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        match &flushed[0].value {
            FlushedValue::Count(v) => assert_eq!(*v, 8000),
            _ => panic!("expected count"),
        }
    }
}
