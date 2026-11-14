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

/// One map entry: the series' key and slot plus flush bookkeeping. Both
/// bookkeeping fields are written only with the stripe lock held.
struct AggEntry {
    key: AggKey,
    slot: AggSlot,
    /// A record hit this entry since the last flush.
    touched: bool,
    /// Consecutive flushes without a record (only counted when retaining).
    idle_flushes: u32,
}

impl AggEntry {
    fn new(key: AggKey, slot: AggSlot) -> Self {
        Self {
            key,
            slot,
            touched: true,
            idle_flushes: 0,
        }
    }
}

type Stripe = Mutex<HashMap<SeriesKey, AggEntry>>;

pub struct StripedAggMap {
    stripes: Box<[Stripe]>,
    max_buckets: u64,
    initial_error: f64,
    aggregate_histograms: bool,
    /// Flushes without a record after which an idle count, histogram or
    /// raw-distribution entry is removed. 0 removes them at every flush.
    retain_idle_flushes: u32,
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
            retain_idle_flushes: 0,
            dropped_raw_samples: AtomicU64::new(0),
            tag_hash_collisions: AtomicU64::new(0),
            raw_sender,
        }
    }

    pub(crate) fn with_histogram_aggregation(mut self, enabled: bool) -> Self {
        self.aggregate_histograms = enabled;
        self
    }

    pub(crate) fn with_retained_idle_flushes(mut self, flushes: u32) -> Self {
        self.retain_idle_flushes = flushes;
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
        tags_match: impl FnOnce(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
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
                let entry = e.into_mut();
                self.check_tags(&entry.key, tags_match);
                self.mark_touched(entry);
                if let AggSlot::Count(ref slot) = entry.slot {
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
                e.insert(AggEntry::new(key, AggSlot::Count(slot)));
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
        tags_match: impl FnOnce(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
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
                // Gauges persist in both modes, so they need no bookkeeping.
                self.check_tags(&e.get().key, tags_match);
                if let AggSlot::Gauge(ref slot) = e.get().slot {
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
                e.insert(AggEntry::new(key, AggSlot::Gauge(slot)));
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
        tags_match: impl FnOnce(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
        value: f64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let metric_name = name.as_str();
        let stripe_idx = combined as usize & STRIPE_MASK;
        let mut guard = self.stripes[stripe_idx].lock();
        let entry = match guard.entry(SeriesKey {
            fingerprint: combined,
            namespace,
            metric_name,
            tags_hash,
            kind: MetricKind::Histogram,
        }) {
            hashbrown::hash_map::Entry::Occupied(e) => {
                let entry = e.into_mut();
                self.check_tags(&entry.key, tags_match);
                entry
            }
            hashbrown::hash_map::Entry::Vacant(e) => {
                let slot = if self.aggregate_histograms {
                    AggSlot::Histogram(HistogramSlot::new(self.max_buckets, self.initial_error))
                } else {
                    AggSlot::RawDistribution
                };
                e.insert(AggEntry::new(
                    AggKey {
                        namespace,
                        metric_name,
                        tags_data: Arc::new(TagsData { pairs: make_tags() }),
                    },
                    slot,
                ))
            }
        };
        self.mark_touched(entry);
        if let AggSlot::Histogram(slot) = &entry.slot {
            slot.record(value);
        }
        let tags = self
            .raw_sender
            .as_ref()
            .map(|_| Arc::clone(&entry.key.tags_data));
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

    /// Series are identified by their 64-bit tag hash. When a recording hits an
    /// existing series, `tags_match` compares the caller's tag values with the
    /// stored ones; a mismatch is a hash collision. It is counted, and the value
    /// is still recorded into the existing series.
    #[inline]
    fn check_tags(
        &self,
        key: &AggKey,
        tags_match: impl FnOnce(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
    ) {
        if !tags_match(&key.tags_data.pairs) {
            self.tag_hash_collisions.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn tag_hash_collisions(&self) -> u64 {
        self.tag_hash_collisions.load(Ordering::Relaxed)
    }

    /// Note that a record hit `entry` since the last flush. Only retention
    /// reads the flag, so with retention off nothing is written, and with it on
    /// the entry is written once per flush instead of on every record.
    #[inline]
    fn mark_touched(&self, entry: &mut AggEntry) {
        if self.retain_idle_flushes > 0 && !entry.touched {
            entry.touched = true;
        }
    }

    pub(crate) fn dropped_raw_samples(&self) -> u64 {
        self.dropped_raw_samples.load(Ordering::Relaxed)
    }

    pub fn flush(&self) -> Vec<FlushedMetric> {
        let mut flushed = Vec::new();

        for stripe in self.stripes.iter() {
            let mut guard = stripe.lock();
            if self.retain_idle_flushes > 0 {
                let max_idle = self.retain_idle_flushes;
                guard.retain(|_combined, entry| flush_retaining(entry, max_idle, &mut flushed));
                continue;
            }
            guard.retain(|_combined, AggEntry { key, slot, .. }| match slot {
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

/// Flush one entry when idle series are retained. Returns whether to keep it.
/// A touched entry is flushed as usual and kept; an untouched one emits
/// nothing and is removed once it has been idle for `max_idle` flushes.
fn flush_retaining(entry: &mut AggEntry, max_idle: u32, flushed: &mut Vec<FlushedMetric>) -> bool {
    let AggEntry {
        key,
        slot,
        touched,
        idle_flushes,
    } = entry;
    if let AggSlot::Gauge(gauge_slot) = slot {
        if let Some(value) = gauge_slot.flush() {
            flushed.push(FlushedMetric {
                namespace: key.namespace,
                metric_name: key.metric_name,
                tags: Arc::clone(&key.tags_data),
                value: FlushedValue::Gauge(value),
            });
        }
        return true;
    }
    if !*touched {
        *idle_flushes = idle_flushes.saturating_add(1);
        return *idle_flushes < max_idle;
    }
    *touched = false;
    *idle_flushes = 0;
    match slot {
        AggSlot::Count(count_slot) => flushed.push(FlushedMetric {
            namespace: key.namespace,
            metric_name: key.metric_name,
            tags: Arc::clone(&key.tags_data),
            value: FlushedValue::Count(count_slot.flush()),
        }),
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
        }
        AggSlot::RawDistribution | AggSlot::Gauge(_) => {}
    }
    true
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

    #[test]
    fn tag_mismatch_on_existing_series_is_counted_and_still_recorded() {
        const NAME: MetricName = MetricName::new("collide");
        let map = make_map();
        let tags = || vec![("k", "a".into())];
        // A new series never consults the comparison.
        map.record_count_named(NAME, &[], 7, tags, |_| panic!("vacant"), 1);
        map.record_gauge_named(NAME, &[], 7, tags, |_| panic!("vacant"), 1.0);
        map.record_histogram_named(NAME, &[], 7, tags, |_| panic!("vacant"), 1.0);
        assert_eq!(map.tag_hash_collisions(), 0);

        // Matching tags on a hit are not collisions.
        let stored = |pairs: &[(&'static str, std::borrow::Cow<'static, str>)]| {
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0], ("k", std::borrow::Cow::Borrowed("a")));
            true
        };
        map.record_count_named(NAME, &[], 7, Vec::new, stored, 1);
        assert_eq!(map.tag_hash_collisions(), 0);

        // Different tags on a hit: counted, and the value still lands in the series.
        map.record_count_named(NAME, &[], 7, Vec::new, |_| false, 10);
        map.record_gauge_named(NAME, &[], 7, Vec::new, |_| false, 5.0);
        map.record_histogram_named(NAME, &[], 7, Vec::new, |_| false, 3.0);
        assert_eq!(map.tag_hash_collisions(), 3);

        let flushed = map.flush();
        assert_eq!(flushed.len(), 3);
        for metric in flushed {
            match metric.value {
                FlushedValue::Count(value) => assert_eq!(value, 12),
                FlushedValue::Gauge(value) => assert_eq!(value, 5.0),
                FlushedValue::Histogram(sketch) => {
                    assert_eq!(sketch.count(), 2);
                    assert_eq!(sketch.sum(), 4.0);
                }
            }
        }
    }

    #[test]
    fn public_recording_functions_never_report_collisions() {
        let map = make_map();
        map.record_count("c", &[], 1, || vec![("k", "a".into())], 1);
        map.record_count("c", &[], 1, || vec![("k", "b".into())], 1);
        assert_eq!(map.tag_hash_collisions(), 0);
    }

    fn entry_count(map: &StripedAggMap) -> usize {
        map.stripes.iter().map(|stripe| stripe.lock().len()).sum()
    }

    fn retaining_map(flushes: u32) -> StripedAggMap {
        make_map().with_retained_idle_flushes(flushes)
    }

    #[test]
    fn default_flush_removes_entries_of_every_kind_except_gauges() {
        let map = make_map();
        map.record_count("c", &[], 1, Vec::new, 1);
        map.record_gauge("g", &[], 2, Vec::new, 1.0);
        map.record_histogram("h", &[], 3, Vec::new, 1.0);
        assert_eq!(entry_count(&map), 3);
        assert_eq!(map.flush().len(), 3);
        assert_eq!(entry_count(&map), 1);
    }

    #[test]
    fn retained_count_keeps_tags_and_emits_only_new_delta() {
        let map = retaining_map(2);
        map.record_count("c", &[], 1, || vec![("k", "v".into())], 5);
        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        assert!(matches!(flushed[0].value, FlushedValue::Count(5)));
        assert_eq!(entry_count(&map), 1);

        map.record_count("c", &[], 1, || panic!("series was retained"), 3);
        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        assert!(matches!(flushed[0].value, FlushedValue::Count(3)));
        assert_eq!(flushed[0].tags.pairs, [("k", "v".into())]);
    }

    #[test]
    fn retained_count_is_evicted_after_idle_flushes() {
        let map = retaining_map(2);
        map.record_count("c", &[], 1, Vec::new, 5);
        assert_eq!(map.flush().len(), 1);
        // Untouched retained counts emit nothing.
        assert_eq!(map.flush().len(), 0);
        assert_eq!(entry_count(&map), 1);
        assert_eq!(map.flush().len(), 0);
        assert_eq!(entry_count(&map), 0);

        let made = std::cell::Cell::new(false);
        map.record_count(
            "c",
            &[],
            1,
            || {
                made.set(true);
                Vec::new()
            },
            1,
        );
        assert!(made.get(), "evicted series registers again");
        assert_eq!(map.flush().len(), 1);
    }

    #[test]
    fn a_record_resets_the_idle_count() {
        let map = retaining_map(2);
        map.record_count("c", &[], 1, Vec::new, 1);
        map.flush();
        map.flush(); // idle 1
        map.record_count("c", &[], 1, || panic!("still retained"), 1);
        assert_eq!(map.flush().len(), 1); // touched: idle back to 0
        map.flush(); // idle 1
        assert_eq!(entry_count(&map), 1);
        map.flush(); // idle 2: removed
        assert_eq!(entry_count(&map), 0);
    }

    #[test]
    fn touched_count_summing_to_zero_still_emits_zero() {
        let map = retaining_map(2);
        map.record_count("c", &[], 1, Vec::new, 4);
        map.record_count("c", &[], 1, Vec::new, -4);
        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        assert!(matches!(flushed[0].value, FlushedValue::Count(0)));
        map.record_count("c", &[], 1, Vec::new, 2);
        map.record_count("c", &[], 1, Vec::new, -2);
        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        assert!(matches!(flushed[0].value, FlushedValue::Count(0)));
        assert_eq!(map.flush().len(), 0);
    }

    #[test]
    fn retained_histogram_emits_only_when_touched_and_is_evicted() {
        let map = retaining_map(2);
        map.record_histogram("h", &[], 1, || vec![("k", "v".into())], 1.0);
        map.record_histogram("h", &[], 1, || panic!("cached"), 3.0);
        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        assert!(matches!(&flushed[0].value, FlushedValue::Histogram(s) if s.count() == 2));

        map.record_histogram("h", &[], 1, || panic!("series was retained"), 10.0);
        let flushed = map.flush();
        assert_eq!(flushed.len(), 1);
        match &flushed[0].value {
            FlushedValue::Histogram(s) => {
                assert_eq!(s.count(), 1);
                assert_eq!(s.sum(), 10.0);
            }
            _ => panic!("expected histogram"),
        }
        assert_eq!(map.flush().len(), 0);
        assert_eq!(entry_count(&map), 1);
        assert_eq!(map.flush().len(), 0);
        assert_eq!(entry_count(&map), 0);
    }

    #[test]
    fn gauges_behave_the_same_when_retaining() {
        let map = retaining_map(1);
        map.record_gauge("g", &[], 1, Vec::new, 7.0);
        for _ in 0..3 {
            let flushed = map.flush();
            assert_eq!(flushed.len(), 1);
            assert!(matches!(flushed[0].value, FlushedValue::Gauge(v) if v == 7.0));
            assert_eq!(entry_count(&map), 1);
        }
        map.record_gauge("g", &[], 1, || panic!("gauge persists"), 8.0);
        assert!(matches!(map.flush()[0].value, FlushedValue::Gauge(v) if v == 8.0));
    }

    #[test]
    fn raw_distribution_entries_are_retained_and_evicted() {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let map = StripedAggMap::new(200, 0.001, Some(tx)).with_retained_idle_flushes(2);
        map.record_histogram("h", &[], 1, || vec![("k", "v".into())], 1.0);
        assert_eq!(entry_count(&map), 1);
        assert_eq!(map.flush().len(), 0, "raw entries never emit");
        assert_eq!(entry_count(&map), 1);
        map.record_histogram("h", &[], 1, || panic!("tags cached"), 2.0);
        assert_eq!(map.flush().len(), 0);
        assert_eq!(entry_count(&map), 1);
        map.flush();
        assert_eq!(entry_count(&map), 1);
        map.flush();
        assert_eq!(entry_count(&map), 0);

        // Without retention the raw entry goes at the first flush.
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let map = StripedAggMap::new(200, 0.001, Some(tx));
        map.record_histogram("h", &[], 1, Vec::new, 1.0);
        map.flush();
        assert_eq!(entry_count(&map), 0);
    }
}
