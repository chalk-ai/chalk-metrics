use std::sync::Arc;

use hashbrown::HashMap;
use parking_lot::Mutex;

use super::count::CountSlot;
use super::gauge::GaugeSlot;
use super::histogram::HistogramSlot;
use crate::__private::MetricName;
use crate::export::{FlushedMetric, FlushedValue, RawDistributionPoint};

const STRIPE_COUNT: usize = 64;
const STRIPE_MASK: usize = 63;

/// Tag data stored in the aggregation map, used by exporters during flush.
/// Contains `(export_name, value)` pairs computed once on insertion.
#[derive(Debug, Clone)]
pub struct TagsData {
    pub pairs: Vec<(&'static str, std::borrow::Cow<'static, str>)>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MetricKind {
    Count,
    Gauge,
    Histogram,
}

// Hashes choose buckets; equality includes the metric's namespace and kind.
// tags_hash is the fingerprint supplied by the generated recording API.
#[derive(Debug, PartialEq, Eq)]
struct SeriesKey {
    fingerprint: u64,
    namespace: &'static [&'static str],
    metric_name: &'static str,
    tags_hash: u64,
    kind: MetricKind,
}

impl std::hash::Hash for SeriesKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // HashMap still owns hashing, including rehashing during growth. Reuse
        // the placement fingerprint, but compare full identity on collisions.
        state.write_u64(self.fingerprint);
    }
}

type Stripe = Mutex<HashMap<SeriesKey, (AggKey, AggSlot)>>;

pub struct StripedAggMap {
    stripes: Box<[Stripe]>,
    max_buckets: u64,
    initial_error: f64,
    raw_sender: Option<tokio::sync::mpsc::Sender<RawDistributionPoint>>,
}

impl StripedAggMap {
    pub fn new(
        max_buckets: u64,
        initial_error: f64,
        raw_sender: Option<tokio::sync::mpsc::Sender<RawDistributionPoint>>,
    ) -> Self {
        let stripes: Vec<_> = (0..STRIPE_COUNT)
            .map(|_| Mutex::new(HashMap::new()))
            .collect();
        Self {
            stripes: stripes.into_boxed_slice(),
            max_buckets,
            initial_error,
            raw_sender,
        }
    }

    #[inline]
    pub fn record_count(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        delta: i64,
    ) {
        self.record_count_named(
            MetricName::new(metric_name),
            namespace,
            tags_hash,
            make_tags,
            delta,
        );
    }

    #[inline]
    pub(crate) fn record_count_named(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        delta: i64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let metric_name = name.as_str();
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();

        let entry = guard.entry(SeriesKey {
            fingerprint: combined,
            namespace,
            metric_name,
            tags_hash,
            kind: MetricKind::Count,
        });

        match entry {
            hashbrown::hash_map::Entry::Occupied(e) => {
                if let AggSlot::Count(ref slot) = e.get().1 {
                    slot.record(delta);
                }
            }
            hashbrown::hash_map::Entry::Vacant(e) => {
                let slot = CountSlot::new();
                slot.record(delta);
                let key = AggKey {
                    namespace,
                    metric_name,
                    tags_data: Arc::new(TagsData { pairs: make_tags() }),
                };
                e.insert((key, AggSlot::Count(slot)));
            }
        }
    }

    #[inline]
    pub fn record_gauge(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        value: f64,
    ) {
        self.record_gauge_named(
            MetricName::new(metric_name),
            namespace,
            tags_hash,
            make_tags,
            value,
        );
    }

    #[inline]
    pub(crate) fn record_gauge_named(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        value: f64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let metric_name = name.as_str();
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();

        let entry = guard.entry(SeriesKey {
            fingerprint: combined,
            namespace,
            metric_name,
            tags_hash,
            kind: MetricKind::Gauge,
        });

        match entry {
            hashbrown::hash_map::Entry::Occupied(e) => {
                if let AggSlot::Gauge(ref slot) = e.get().1 {
                    slot.record(value);
                }
            }
            hashbrown::hash_map::Entry::Vacant(e) => {
                let slot = GaugeSlot::new();
                slot.record(value);
                let key = AggKey {
                    namespace,
                    metric_name,
                    tags_data: Arc::new(TagsData { pairs: make_tags() }),
                };
                e.insert((key, AggSlot::Gauge(slot)));
            }
        }
    }

    #[inline]
    pub fn record_histogram(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        value: f64,
    ) {
        self.record_histogram_named(
            MetricName::new(metric_name),
            namespace,
            tags_hash,
            make_tags,
            value,
        );
    }

    #[inline]
    pub(crate) fn record_histogram_named(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> Vec<(&'static str, std::borrow::Cow<'static, str>)>,
        value: f64,
    ) {
        let metric_name = name.as_str();
        match &self.raw_sender {
            Some(sender) => {
                let tags = Arc::new(TagsData { pairs: make_tags() });
                let _ = sender.try_send(RawDistributionPoint {
                    namespace,
                    metric_name,
                    tags,
                    value,
                });
            }
            None => {
                let combined = combine_hash(name.hash(), tags_hash);
                let stripe_idx = combined as usize & STRIPE_MASK;
                let mut guard = self.stripes[stripe_idx].lock();

                let entry = guard.entry(SeriesKey {
                    fingerprint: combined,
                    namespace,
                    metric_name,
                    tags_hash,
                    kind: MetricKind::Histogram,
                });

                match entry {
                    hashbrown::hash_map::Entry::Occupied(e) => {
                        if let AggSlot::Histogram(ref slot) = e.get().1 {
                            slot.record(value);
                        }
                    }
                    hashbrown::hash_map::Entry::Vacant(e) => {
                        let slot = HistogramSlot::new(self.max_buckets, self.initial_error);
                        slot.record(value);
                        let key = AggKey {
                            namespace,
                            metric_name,
                            tags_data: Arc::new(TagsData { pairs: make_tags() }),
                        };
                        e.insert((key, AggSlot::Histogram(slot)));
                    }
                }
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

/// Placement hint only: full series identity is compared by HashMap::entry.
/// Equal metric names must select the same stripe even at different addresses.
#[inline]
fn combine_hash(name_hash: u64, tags_hash: u64) -> u64 {
    tags_hash
        .wrapping_mul(6364136223846793005)
        .wrapping_add(name_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_map() -> StripedAggMap {
        StripedAggMap::new(200, 0.001, None)
    }

    fn record_growth(map: &StripedAggMap, kind: u8, series: u64, value: f64) {
        let tags = || vec![("series", series.to_string().into())];
        // Multiples of 64 force every key into one stripe in the original map.
        match kind {
            0 => map.record_count("growth", &[], series * 64, tags, value as i64),
            1 => map.record_gauge("growth", &[], series * 64, tags, value),
            _ => map.record_histogram("growth", &[], series * 64, tags, value),
        }
    }

    fn check_growth(kind: u8) {
        let map = make_map();
        for series in 0..512 {
            record_growth(&map, kind, series, 1.0);
        }
        // Assert that the workload really exercised stripe growth.
        assert!(map.stripes.iter().any(|stripe| stripe.lock().len() == 512));
        for series in 0..512 {
            record_growth(&map, kind, series, 99.0);
        }
        let flushed = map.flush();
        assert_eq!(flushed.len(), 512, "one entry per series after growth");
        let mut seen = std::collections::HashSet::new();
        for metric in flushed {
            assert!(seen.insert(metric.tags.pairs[0].1.clone()));
            match metric.value {
                FlushedValue::Count(value) => assert_eq!(value, 100),
                FlushedValue::Gauge(value) => assert_eq!(value, 99.0),
                FlushedValue::Histogram(sketch) => {
                    assert_eq!(sketch.count(), 2);
                    assert_eq!(sketch.sum(), 100.0);
                    assert_eq!(sketch.min(), 1.0);
                    assert_eq!(sketch.max(), 99.0);
                }
            }
        }
        assert_eq!(map.flush().len(), if kind == 1 { 512 } else { 0 });
        for series in 0..512 {
            record_growth(&map, kind, series, 7.0);
        }
        assert_eq!(map.flush().len(), 512);
    }

    #[test]
    fn growth_preserves_counter_totals_and_unique_series() {
        check_growth(0);
    }

    #[test]
    fn growth_preserves_gauge_updates() {
        check_growth(1);
    }

    #[test]
    fn growth_preserves_histogram_samples() {
        check_growth(2);
    }

    #[test]
    fn same_name_in_different_namespaces_is_distinct() {
        let map = make_map();
        map.record_count("requests", &["http"], 42, Vec::new, 1);
        map.record_count("requests", &["rpc"], 42, Vec::new, 99);
        assert_eq!(map.flush().len(), 2);
    }

    #[test]
    fn metric_names_compare_by_content() {
        let map = make_map();
        let first = Box::leak(String::from("same_name").into_boxed_str());
        let second = Box::leak(String::from("same_name").into_boxed_str());
        assert_ne!(first.as_ptr(), second.as_ptr());
        map.record_count(first, &[], 42, Vec::new, 1);
        map.record_count(second, &[], 42, || panic!("tags already cached"), 99);
        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        assert!(matches!(flushed[0].value, FlushedValue::Count(100)));
    }

    #[test]
    fn precomputed_and_runtime_names_coalesce_after_growth() {
        const NAME: MetricName = MetricName::new("growth");
        let map = make_map();
        let other_name = Box::leak(String::from("growth").into_boxed_str());
        assert_ne!(NAME.as_str().as_ptr(), other_name.as_ptr());
        for series in 0..512 {
            map.record_count_named(NAME, &[], series * 64, Vec::new, 1);
            map.record_gauge_named(NAME, &[], series * 64, Vec::new, 1.0);
            map.record_histogram_named(NAME, &[], series * 64, Vec::new, 1.0);
        }
        assert!(map.stripes.iter().any(|stripe| stripe.lock().len() == 1536));
        for series in 0..512 {
            let cached = || panic!("precomputed and runtime names must share cached tags");
            map.record_count(other_name, &[], series * 64, cached, 99);
            map.record_gauge(other_name, &[], series * 64, cached, 99.0);
            map.record_histogram(other_name, &[], series * 64, cached, 99.0);
        }
        let flushed = map.flush();
        assert_eq!(flushed.len(), 1536);
        for metric in flushed {
            match metric.value {
                FlushedValue::Count(value) => assert_eq!(value, 100),
                FlushedValue::Gauge(value) => assert_eq!(value, 99.0),
                FlushedValue::Histogram(sketch) => {
                    assert_eq!(sketch.count(), 2);
                    assert_eq!(sketch.sum(), 100.0);
                    assert_eq!(sketch.min(), 1.0);
                    assert_eq!(sketch.max(), 99.0);
                }
            }
        }
    }

    #[test]
    fn concurrent_growth_keeps_one_counter_per_series() {
        let map = Arc::new(make_map());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let map = Arc::clone(&map);
                std::thread::spawn(move || {
                    for _ in 0..4 {
                        for series in 0..512 {
                            record_growth(&map, 0, series, 1.0);
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let flushed = map.flush();
        assert_eq!(flushed.len(), 512);
        for metric in flushed {
            assert!(matches!(metric.value, FlushedValue::Count(32)));
        }
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
        map.record_count("request_count", &["http"], 100, Vec::new, 1);

        let flushed = map.flush();
        assert_eq!(flushed[0].namespace, &["http"]);
        assert_eq!(flushed[0].metric_name, "request_count");
    }

    #[test]
    fn test_insert_and_record_gauge() {
        let map = make_map();
        map.record_gauge("test_gauge", &[], 456, Vec::new, 10.0);
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
        map.record_histogram("test_hist", &[], 789, Vec::new, 42.0);
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
        map.record_count("count_a", &[], 100, Vec::new, 1);
        map.record_count("count_b", &[], 100, Vec::new, 2);
        assert_eq!(map.flush().len(), 2);
    }

    #[test]
    fn test_flush_removes_count_entries() {
        let map = make_map();
        map.record_count("c", &[], 100, Vec::new, 5);
        assert_eq!(map.flush().len(), 1);
        assert_eq!(map.flush().len(), 0);
    }

    #[test]
    fn test_flush_retains_gauge_entries() {
        let map = make_map();
        map.record_gauge("g", &[], 100, Vec::new, 42.0);
        assert_eq!(map.flush().len(), 1);
        assert_eq!(map.flush().len(), 1);
    }

    #[test]
    fn test_histogram_passthrough_sends_raw_points_without_aggregating() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let map = StripedAggMap::new(200, 0.001, Some(tx));

        // Passthrough mode never touches the aggregation map, so make_tags is
        // called fresh on every record (no cached Arc<TagsData> to reuse).
        map.record_histogram("test_hist", &[], 789, || vec![("k", "v".into())], 1.0);
        map.record_histogram("test_hist", &[], 789, || vec![("k", "v".into())], 2.0);

        // The sketch is bypassed entirely in passthrough mode, so flush() has nothing to emit.
        let flushed = map.flush();
        assert_eq!(flushed.len(), 0);

        let point1 = rx.try_recv().expect("expected first raw point");
        let point2 = rx.try_recv().expect("expected second raw point");
        assert!(rx.try_recv().is_err());

        assert_eq!(point1.value, 1.0);
        assert_eq!(point2.value, 2.0);
        assert_eq!(point1.metric_name, "test_hist");
        assert_eq!(point1.tags.pairs, point2.tags.pairs);
    }

    #[test]
    fn test_histogram_without_raw_sender_sends_nothing() {
        let map = make_map();
        map.record_histogram("test_hist", &[], 789, Vec::new, 1.0);
        assert!(map.raw_sender.is_none());
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
