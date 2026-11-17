use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

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
    RawDistribution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum MetricKind {
    Count,
    Gauge,
    Histogram,
}

/// Most series kept apart under one tag hash. Colliding tag sets take the next
/// probe; if all are taken by other tag sets, a recording joins the last one.
/// This bounds memory and lookup time (gauges persist forever) even if a
/// `tags_match` were inconsistent with the stored tags.
const MAX_COLLIDING_SERIES: u32 = 8;

// Hashes choose buckets; equality includes the metric's namespace and kind.
// tags_hash is the fingerprint supplied by the generated recording API, and
// probe separates tag sets that share it (0 for the first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SeriesKey {
    fingerprint: u64,
    namespace: &'static [&'static str],
    metric_name: &'static str,
    tags_hash: u64,
    kind: MetricKind,
    probe: u32,
}

impl std::hash::Hash for SeriesKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // HashMap still owns hashing, including rehashing during growth. Reuse
        // the placement fingerprint (probes of one hash share a stripe), but
        // compare full identity on collisions.
        state.write_u64(self.fingerprint);
    }
}

type Stripe = Mutex<HashMap<SeriesKey, (AggKey, AggSlot)>>;

pub struct StripedAggMap {
    stripes: Box<[Stripe]>,
    max_buckets: u64,
    initial_error: f64,
    aggregate_histograms: bool,
    dropped_raw_samples: AtomicU64,
    tag_hash_collisions: AtomicU64,
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
            aggregate_histograms: raw_sender.is_none(),
            dropped_raw_samples: AtomicU64::new(0),
            tag_hash_collisions: AtomicU64::new(0),
            raw_sender,
        }
    }

    pub(crate) fn with_histogram_aggregation(mut self, enabled: bool) -> Self {
        self.aggregate_histograms = enabled;
        self
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
            |_| true,
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
        tags_match: impl Fn(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
        delta: i64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let metric_name = name.as_str();
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();
        let key = SeriesKey {
            fingerprint: combined,
            namespace,
            metric_name,
            tags_hash,
            kind: MetricKind::Count,
            probe: 0,
        };

        match guard.entry(key) {
            hashbrown::hash_map::Entry::Occupied(e) => {
                if tags_match(&e.get().0.tags_data.pairs) {
                    if let AggSlot::Count(ref slot) = e.get().1 {
                        slot.record(delta);
                    }
                    return;
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
                return;
            }
        }
        let (_, slot) = self.colliding_series(&mut guard, key, make_tags());
        if let AggSlot::Count(slot) = slot {
            slot.record(delta);
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
            |_| true,
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
        tags_match: impl Fn(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
        value: f64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let metric_name = name.as_str();
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();
        let key = SeriesKey {
            fingerprint: combined,
            namespace,
            metric_name,
            tags_hash,
            kind: MetricKind::Gauge,
            probe: 0,
        };

        match guard.entry(key) {
            hashbrown::hash_map::Entry::Occupied(e) => {
                if tags_match(&e.get().0.tags_data.pairs) {
                    if let AggSlot::Gauge(ref slot) = e.get().1 {
                        slot.record(value);
                    }
                    return;
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
                return;
            }
        }
        let (_, slot) = self.colliding_series(&mut guard, key, make_tags());
        if let AggSlot::Gauge(slot) = slot {
            slot.record(value);
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
            |_| true,
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
        tags_match: impl Fn(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
        value: f64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let metric_name = name.as_str();
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();
        let key = SeriesKey {
            fingerprint: combined,
            namespace,
            metric_name,
            tags_hash,
            kind: MetricKind::Histogram,
            probe: 0,
        };
        // Used by exactly one of the arms below; the compiler cannot see that.
        let mut make_tags = Some(make_tags);
        let (hit, tags) = match guard.entry(key) {
            hashbrown::hash_map::Entry::Occupied(e) => {
                let (stored, slot) = e.get();
                if tags_match(&stored.tags_data.pairs) {
                    if let AggSlot::Histogram(slot) = slot {
                        slot.record(value);
                    }
                    let tags = self
                        .raw_sender
                        .as_ref()
                        .map(|_| Arc::clone(&stored.tags_data));
                    (true, tags)
                } else {
                    (false, None)
                }
            }
            hashbrown::hash_map::Entry::Vacant(e) => {
                let (stored, slot) = e.insert((
                    AggKey {
                        namespace,
                        metric_name,
                        tags_data: Arc::new(TagsData {
                            pairs: make_tags.take().unwrap()(),
                        }),
                    },
                    self.new_slot(MetricKind::Histogram),
                ));
                if let AggSlot::Histogram(slot) = slot {
                    slot.record(value);
                }
                let tags = self
                    .raw_sender
                    .as_ref()
                    .map(|_| Arc::clone(&stored.tags_data));
                (true, tags)
            }
        };
        let tags = if hit {
            tags
        } else {
            let (stored, slot) =
                self.colliding_series(&mut guard, key, make_tags.take().unwrap()());
            if let AggSlot::Histogram(slot) = slot {
                slot.record(value);
            }
            self.raw_sender
                .as_ref()
                .map(|_| Arc::clone(&stored.tags_data))
        };
        drop(guard);
        if let (Some(sender), Some(tags)) = (&self.raw_sender, tags)
            && sender
                .try_send(RawDistributionPoint {
                    namespace,
                    metric_name,
                    tags,
                    value,
                })
                .is_err()
        {
            self.dropped_raw_samples.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Cold path of the `record_*_named` functions: probe 0 of `key` holds a
    /// different tag set (a tag-hash collision), so find or create this tag
    /// set's series at the next probes and return it. Counts the collision once.
    /// If all `MAX_COLLIDING_SERIES` probes hold other tag sets, the last one is
    /// returned. The caller passes the tags eagerly, so recording into a
    /// colliding series builds its tags every time. Not generic, so it is
    /// compiled once rather than once per metric.
    #[cold]
    #[inline(never)]
    fn colliding_series<'a>(
        &self,
        map: &'a mut HashMap<SeriesKey, (AggKey, AggSlot)>,
        mut key: SeriesKey,
        pairs: Vec<(&'static str, std::borrow::Cow<'static, str>)>,
    ) -> &'a (AggKey, AggSlot) {
        self.tag_hash_collisions.fetch_add(1, Ordering::Relaxed);
        for probe in 1..MAX_COLLIDING_SERIES {
            key.probe = probe;
            match map.get(&key) {
                Some((stored, _))
                    if probe + 1 < MAX_COLLIDING_SERIES && stored.tags_data.pairs != pairs => {}
                _ => break,
            }
        }
        map.entry(key).or_insert_with(|| {
            (
                AggKey {
                    namespace: key.namespace,
                    metric_name: key.metric_name,
                    tags_data: Arc::new(TagsData { pairs }),
                },
                self.new_slot(key.kind),
            )
        })
    }

    fn new_slot(&self, kind: MetricKind) -> AggSlot {
        match kind {
            MetricKind::Count => AggSlot::Count(CountSlot::new()),
            MetricKind::Gauge => AggSlot::Gauge(GaugeSlot::new()),
            MetricKind::Histogram if self.aggregate_histograms => {
                AggSlot::Histogram(HistogramSlot::new(self.max_buckets, self.initial_error))
            }
            MetricKind::Histogram => AggSlot::RawDistribution,
        }
    }

    pub(crate) fn tag_hash_collisions(&self) -> u64 {
        self.tag_hash_collisions.load(Ordering::Relaxed)
    }

    pub(crate) fn dropped_raw_samples(&self) -> u64 {
        self.dropped_raw_samples.load(Ordering::Relaxed)
    }

    pub fn flush(&self) -> Vec<FlushedMetric> {
        let mut flushed = Vec::new();

        for stripe in self.stripes.iter() {
            let mut guard = stripe.lock();
            guard.retain(|_combined, (key, slot)| match slot {
                AggSlot::RawDistribution => false,
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
            map.record_count_named(NAME, &[], series * 64, Vec::new, |_| true, 1);
            map.record_gauge_named(NAME, &[], series * 64, Vec::new, |_| true, 1.0);
            map.record_histogram_named(NAME, &[], series * 64, Vec::new, |_| true, 1.0);
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

        // Raw-only entries cache tags but do not allocate histogram sketches.
        map.record_histogram("test_hist", &[], 789, || vec![("k", "v".into())], 1.0);
        map.record_histogram("test_hist", &[], 789, || panic!("tags cached"), 2.0);

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

    type Pairs = Vec<(&'static str, std::borrow::Cow<'static, str>)>;

    const COLLIDE: MetricName = MetricName::new("collide");

    /// Tags of the test series `v`: one pair, `k = v`.
    fn tags_of(v: &'static str) -> impl Fn() -> Pairs + Copy {
        move || vec![("k", v.into())]
    }

    /// A real comparison of stored tags with the series `v`.
    fn matches(
        v: &'static str,
    ) -> impl Fn(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool + Copy {
        move |pairs| pairs.len() == 1 && pairs[0].0 == "k" && pairs[0].1 == v
    }

    fn rec_count(map: &StripedAggMap, v: &'static str, delta: i64) {
        map.record_count_named(COLLIDE, &[], 7, tags_of(v), matches(v), delta);
    }

    fn rec_gauge(map: &StripedAggMap, v: &'static str, value: f64) {
        map.record_gauge_named(COLLIDE, &[], 7, tags_of(v), matches(v), value);
    }

    fn rec_hist(map: &StripedAggMap, v: &'static str, value: f64) {
        map.record_histogram_named(COLLIDE, &[], 7, tags_of(v), matches(v), value);
    }

    /// Flushed series as `(tag value, kind) -> value` (histograms: sum).
    fn flushed_series(
        map: &StripedAggMap,
    ) -> std::collections::BTreeMap<(String, &'static str), f64> {
        let mut out = std::collections::BTreeMap::new();
        for metric in map.flush() {
            assert_eq!(metric.tags.pairs.len(), 1);
            let tag = metric.tags.pairs[0].1.to_string();
            let (kind, value) = match metric.value {
                FlushedValue::Count(v) => ("count", v as f64),
                FlushedValue::Gauge(v) => ("gauge", v),
                FlushedValue::Histogram(sketch) => ("hist", sketch.sum()),
            };
            assert!(out.insert((tag, kind), value).is_none(), "duplicate series");
        }
        out
    }

    fn series_of(
        map: &std::collections::BTreeMap<(String, &'static str), f64>,
        tag: &str,
        kind: &'static str,
    ) -> f64 {
        *map.get(&(tag.to_owned(), kind))
            .unwrap_or_else(|| panic!("no {kind} series {tag}"))
    }

    #[test]
    fn colliding_tag_sets_are_separate_series() {
        let map = make_map();
        rec_count(&map, "a", 1);
        rec_count(&map, "b", 10);
        rec_gauge(&map, "a", 1.5);
        rec_gauge(&map, "b", 2.5);
        rec_hist(&map, "a", 3.0);
        rec_hist(&map, "b", 4.0);
        rec_hist(&map, "b", 5.0);

        let flushed = flushed_series(&map);
        assert_eq!(flushed.len(), 6);
        assert_eq!(series_of(&flushed, "a", "count"), 1.0);
        assert_eq!(series_of(&flushed, "b", "count"), 10.0);
        assert_eq!(series_of(&flushed, "a", "gauge"), 1.5);
        assert_eq!(series_of(&flushed, "b", "gauge"), 2.5);
        assert_eq!(series_of(&flushed, "a", "hist"), 3.0);
        assert_eq!(series_of(&flushed, "b", "hist"), 9.0);
    }

    #[test]
    fn colliding_tag_sets_send_their_own_tags_in_raw_mode() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let map = StripedAggMap::new(200, 0.001, Some(tx));
        rec_hist(&map, "a", 1.0);
        rec_hist(&map, "b", 2.0);
        rec_hist(&map, "a", 3.0);
        rec_hist(&map, "b", 4.0);

        let mut got = Vec::new();
        while let Ok(point) = rx.try_recv() {
            assert_eq!(point.tags.pairs.len(), 1);
            got.push((point.tags.pairs[0].1.to_string(), point.value));
        }
        let expected: Vec<_> = [("a", 1.0), ("b", 2.0), ("a", 3.0), ("b", 4.0)]
            .into_iter()
            .map(|(tag, value)| (tag.to_owned(), value))
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn interleaved_colliding_recordings_keep_exact_totals() {
        let map = make_map();
        rec_count(&map, "a", 1);
        rec_count(&map, "b", 10);
        rec_count(&map, "a", 2);
        rec_count(&map, "b", 20);
        let flushed = flushed_series(&map);
        assert_eq!(flushed.len(), 2);
        assert_eq!(series_of(&flushed, "a", "count"), 3.0);
        assert_eq!(series_of(&flushed, "b", "count"), 30.0);
    }

    #[test]
    fn colliding_series_are_correct_after_a_flush_in_either_order() {
        let map = make_map();
        rec_count(&map, "a", 1);
        rec_count(&map, "b", 10);
        assert_eq!(map.flush().len(), 2);

        rec_count(&map, "b", 20);
        rec_count(&map, "a", 2);
        let flushed = flushed_series(&map);
        assert_eq!(flushed.len(), 2);
        assert_eq!(series_of(&flushed, "a", "count"), 2.0);
        assert_eq!(series_of(&flushed, "b", "count"), 20.0);
    }

    #[test]
    fn colliding_gauges_persist_as_two_series() {
        let map = make_map();
        rec_gauge(&map, "a", 1.5);
        rec_gauge(&map, "b", 2.5);
        for _ in 0..2 {
            let flushed = flushed_series(&map);
            assert_eq!(flushed.len(), 2);
            assert_eq!(series_of(&flushed, "a", "gauge"), 1.5);
            assert_eq!(series_of(&flushed, "b", "gauge"), 2.5);
        }
    }

    #[test]
    fn three_tag_sets_under_one_hash_are_three_series() {
        let map = make_map();
        for (tag, delta) in [("a", 1), ("b", 2), ("c", 3), ("a", 10), ("c", 30)] {
            rec_count(&map, tag, delta);
        }
        let flushed = flushed_series(&map);
        assert_eq!(flushed.len(), 3);
        assert_eq!(series_of(&flushed, "a", "count"), 11.0);
        assert_eq!(series_of(&flushed, "b", "count"), 2.0);
        assert_eq!(series_of(&flushed, "c", "count"), 33.0);
    }

    #[test]
    fn colliding_series_are_bounded_and_nothing_is_lost() {
        const TAGS: [&str; 10] = ["t0", "t1", "t2", "t3", "t4", "t5", "t6", "t7", "t8", "t9"];
        let map = make_map();
        for tag in TAGS {
            rec_count(&map, tag, 1);
            rec_gauge(&map, tag, 1.0);
        }
        let flushed = flushed_series(&map);
        assert_eq!(flushed.len(), 16, "eight count and eight gauge series");
        for tag in &TAGS[..7] {
            assert_eq!(series_of(&flushed, tag, "count"), 1.0);
        }
        // t8 and t9 share the last series with t7.
        assert_eq!(series_of(&flushed, "t7", "count"), 3.0);
        let total: f64 = flushed
            .iter()
            .filter(|((_, kind), _)| *kind == "count")
            .map(|(_, value)| value)
            .sum();
        assert_eq!(total, 10.0);
        assert!(!flushed.contains_key(&("t8".to_owned(), "count")));
        assert!(!flushed.contains_key(&("t9".to_owned(), "count")));
    }

    #[test]
    fn collision_counter_counts_recordings_that_step_past_another_tag_set() {
        let map = make_map();
        rec_count(&map, "a", 1);
        rec_count(&map, "a", 1);
        rec_gauge(&map, "a", 1.0);
        rec_hist(&map, "a", 1.0);
        assert_eq!(map.tag_hash_collisions(), 0, "nothing collides");

        rec_count(&map, "b", 1);
        assert_eq!(map.tag_hash_collisions(), 1);
        rec_count(&map, "b", 1);
        assert_eq!(map.tag_hash_collisions(), 2);
        rec_count(&map, "a", 1);
        assert_eq!(map.tag_hash_collisions(), 2, "first series is not a step");

        rec_count(&map, "c", 1);
        assert_eq!(
            map.tag_hash_collisions(),
            3,
            "one per recording, not per step"
        );
    }

    #[test]
    fn concurrent_recordings_of_colliding_tag_sets_are_exact() {
        let map = Arc::new(make_map());
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let map = Arc::clone(&map);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        rec_count(&map, "a", 1);
                        rec_count(&map, "b", 2);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let flushed = flushed_series(&map);
        assert_eq!(flushed.len(), 2);
        assert_eq!(series_of(&flushed, "a", "count"), 4000.0);
        assert_eq!(series_of(&flushed, "b", "count"), 8000.0);
    }

    #[test]
    fn a_new_series_is_not_compared_and_a_hit_is_compared_once() {
        let map = make_map();
        let calls = std::cell::Cell::new(0u32);
        let counted = |pairs: &[(&'static str, std::borrow::Cow<'static, str>)]| {
            calls.set(calls.get() + 1);
            matches("a")(pairs)
        };
        map.record_count_named(COLLIDE, &[], 7, tags_of("a"), counted, 1);
        map.record_gauge_named(COLLIDE, &[], 7, tags_of("a"), counted, 1.0);
        map.record_histogram_named(COLLIDE, &[], 7, tags_of("a"), counted, 1.0);
        assert_eq!(calls.get(), 0, "a new series is not compared");

        map.record_count_named(COLLIDE, &[], 7, tags_of("a"), counted, 1);
        map.record_gauge_named(COLLIDE, &[], 7, tags_of("a"), counted, 1.0);
        map.record_histogram_named(COLLIDE, &[], 7, tags_of("a"), counted, 1.0);
        assert_eq!(calls.get(), 3, "a hit is compared once");
        assert_eq!(map.tag_hash_collisions(), 0);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn series_key_stays_56_bytes() {
        assert_eq!(std::mem::size_of::<SeriesKey>(), 56);
    }

    #[test]
    fn public_recording_functions_never_report_collisions() {
        let map = make_map();
        map.record_count("c", &[], 1, || vec![("k", "a".into())], 1);
        map.record_count("c", &[], 1, || vec![("k", "b".into())], 1);
        assert_eq!(map.tag_hash_collisions(), 0);
    }
}
