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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagsData {
    pub pairs: Vec<(&'static str, std::borrow::Cow<'static, str>)>,
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

// Hashes choose buckets; equality compares the full series identity, including tags.
#[derive(Debug, PartialEq, Eq)]
struct SeriesKey {
    fingerprint: u64,
    namespace: &'static [&'static str],
    metric_name: &'static str,
    tags_hash: u64,
    kind: MetricKind,
    tags_data: Arc<TagsData>,
}

impl std::hash::Hash for SeriesKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // HashMap still owns hashing, including rehashing during growth. Reuse
        // the placement fingerprint, but compare full identity on collisions.
        state.write_u64(self.fingerprint);
    }
}

type TagPairs = Vec<(&'static str, std::borrow::Cow<'static, str>)>;
type Stripe = Mutex<HashMap<SeriesKey, AggSlot>>;

trait RecordTags {
    fn matches(&self, pairs: &TagPairs) -> bool;
    fn into_pairs(self) -> TagPairs;
}

impl RecordTags for TagPairs {
    fn matches(&self, pairs: &TagPairs) -> bool {
        self == pairs
    }

    fn into_pairs(self) -> TagPairs {
        self
    }
}

struct BorrowedTags<F, M> {
    matches: F,
    make: M,
}

impl<F, M> RecordTags for BorrowedTags<F, M>
where
    F: Fn(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
    M: FnOnce() -> TagPairs,
{
    fn matches(&self, pairs: &TagPairs) -> bool {
        (self.matches)(pairs)
    }

    fn into_pairs(self) -> TagPairs {
        (self.make)()
    }
}

// Lookup and stored keys hash the same placement fingerprint. A matching hash
// only identifies candidates; equality always checks every exported tag pair.
struct SeriesLookup<'a, T> {
    fingerprint: u64,
    namespace: &'static [&'static str],
    metric_name: &'static str,
    tags_hash: u64,
    kind: MetricKind,
    tags: &'a T,
}

impl<T> std::hash::Hash for SeriesLookup<'_, T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.fingerprint);
    }
}

impl<T: RecordTags> hashbrown::Equivalent<SeriesKey> for SeriesLookup<'_, T> {
    fn equivalent(&self, key: &SeriesKey) -> bool {
        self.fingerprint == key.fingerprint
            && self.namespace == key.namespace
            && self.metric_name == key.metric_name
            && self.tags_hash == key.tags_hash
            && self.kind == key.kind
            && self.tags.matches(&key.tags_data.pairs)
    }
}

impl SeriesKey {
    fn new(
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        kind: MetricKind,
        tags: TagPairs,
    ) -> Self {
        Self {
            fingerprint: combine_hash(name.hash(), tags_hash),
            namespace,
            metric_name: name.as_str(),
            tags_hash,
            kind,
            tags_data: Arc::new(TagsData { pairs: tags }),
        }
    }
}

pub struct StripedAggMap {
    stripes: Box<[Stripe]>,
    max_buckets: u64,
    initial_error: f64,
    aggregate_histograms: bool,
    dropped_raw_samples: AtomicU64,
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
            raw_sender,
        }
    }

    pub(crate) fn with_histogram_aggregation(mut self, enabled: bool) -> Self {
        self.aggregate_histograms = enabled;
        self
    }

    /// Record a count, materializing tags on every call to compare full identity.
    /// Macro-generated metrics use a borrowed comparison on cache hits instead.
    #[inline]
    pub fn record_count(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> TagPairs,
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
        make_tags: impl FnOnce() -> TagPairs,
        delta: i64,
    ) {
        self.record_count_with_tags(name, namespace, tags_hash, make_tags(), delta);
    }

    #[inline]
    pub(crate) fn record_count_matched(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        matches: impl Fn(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
        make: impl FnOnce() -> TagPairs,
        delta: i64,
    ) {
        self.record_count_with_tags(
            name,
            namespace,
            tags_hash,
            BorrowedTags { matches, make },
            delta,
        );
    }

    #[inline]
    fn record_count_with_tags<T: RecordTags>(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        tags: T,
        delta: i64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let query = SeriesLookup {
            fingerprint: combined,
            namespace,
            metric_name: name.as_str(),
            tags_hash,
            kind: MetricKind::Count,
            tags: &tags,
        };
        let mut guard = self.stripes[combined as usize & STRIPE_MASK].lock();
        if let Some(AggSlot::Count(slot)) = guard.get_mut(&query) {
            slot.record(delta);
        } else {
            let slot = CountSlot::new();
            slot.record(delta);
            let key = SeriesKey::new(
                name,
                namespace,
                tags_hash,
                MetricKind::Count,
                tags.into_pairs(),
            );
            guard.insert(key, AggSlot::Count(slot));
        }
    }

    /// Record a gauge, materializing tags on every call to compare full identity.
    /// Macro-generated metrics use a borrowed comparison on cache hits instead.
    #[inline]
    pub fn record_gauge(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> TagPairs,
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
        make_tags: impl FnOnce() -> TagPairs,
        value: f64,
    ) {
        self.record_gauge_with_tags(name, namespace, tags_hash, make_tags(), value);
    }

    #[inline]
    pub(crate) fn record_gauge_matched(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        matches: impl Fn(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
        make: impl FnOnce() -> TagPairs,
        value: f64,
    ) {
        self.record_gauge_with_tags(
            name,
            namespace,
            tags_hash,
            BorrowedTags { matches, make },
            value,
        );
    }

    #[inline]
    fn record_gauge_with_tags<T: RecordTags>(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        tags: T,
        value: f64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let query = SeriesLookup {
            fingerprint: combined,
            namespace,
            metric_name: name.as_str(),
            tags_hash,
            kind: MetricKind::Gauge,
            tags: &tags,
        };
        let mut guard = self.stripes[combined as usize & STRIPE_MASK].lock();
        if let Some(AggSlot::Gauge(slot)) = guard.get_mut(&query) {
            slot.record(value);
        } else {
            let slot = GaugeSlot::new();
            slot.record(value);
            let key = SeriesKey::new(
                name,
                namespace,
                tags_hash,
                MetricKind::Gauge,
                tags.into_pairs(),
            );
            guard.insert(key, AggSlot::Gauge(slot));
        }
    }

    /// Record a histogram, materializing tags on every call to compare full identity.
    /// Macro-generated metrics use a borrowed comparison on cache hits instead.
    #[inline]
    pub fn record_histogram(
        &self,
        metric_name: &'static str,
        namespace: &'static [&'static str],
        tags_hash: u64,
        make_tags: impl FnOnce() -> TagPairs,
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
        make_tags: impl FnOnce() -> TagPairs,
        value: f64,
    ) {
        self.record_histogram_with_tags(name, namespace, tags_hash, make_tags(), value);
    }

    #[inline]
    pub(crate) fn record_histogram_matched(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        matches: impl Fn(&[(&'static str, std::borrow::Cow<'static, str>)]) -> bool,
        make: impl FnOnce() -> TagPairs,
        value: f64,
    ) {
        self.record_histogram_with_tags(
            name,
            namespace,
            tags_hash,
            BorrowedTags { matches, make },
            value,
        );
    }

    #[inline]
    fn record_histogram_with_tags<T: RecordTags>(
        &self,
        name: MetricName,
        namespace: &'static [&'static str],
        tags_hash: u64,
        tags: T,
        value: f64,
    ) {
        let combined = combine_hash(name.hash(), tags_hash);
        let query = SeriesLookup {
            fingerprint: combined,
            namespace,
            metric_name: name.as_str(),
            tags_hash,
            kind: MetricKind::Histogram,
            tags: &tags,
        };
        let mut guard = self.stripes[combined as usize & STRIPE_MASK].lock();
        let raw_tags = if let Some((key, slot)) = guard.get_key_value_mut(&query) {
            if let AggSlot::Histogram(slot) = slot {
                slot.record(value);
            }
            self.raw_sender.as_ref().map(|_| Arc::clone(&key.tags_data))
        } else {
            let slot = if self.aggregate_histograms {
                let slot = HistogramSlot::new(self.max_buckets, self.initial_error);
                slot.record(value);
                AggSlot::Histogram(slot)
            } else {
                AggSlot::RawDistribution
            };
            let key = SeriesKey::new(
                name,
                namespace,
                tags_hash,
                MetricKind::Histogram,
                tags.into_pairs(),
            );
            let raw_tags = self.raw_sender.as_ref().map(|_| Arc::clone(&key.tags_data));
            guard.insert(key, slot);
            raw_tags
        };
        drop(guard);
        if let (Some(sender), Some(tags)) = (&self.raw_sender, raw_tags)
            && sender
                .try_send(RawDistributionPoint {
                    namespace,
                    metric_name: name.as_str(),
                    tags,
                    value,
                })
                .is_err()
        {
            self.dropped_raw_samples.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn dropped_raw_samples(&self) -> u64 {
        self.dropped_raw_samples.load(Ordering::Relaxed)
    }

    pub fn flush(&self) -> Vec<FlushedMetric> {
        let mut flushed = Vec::new();

        for stripe in self.stripes.iter() {
            let mut guard = stripe.lock();
            guard.retain(|key, slot| match slot {
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

/// Placement hint only: lookup compares the full series identity, including tags.
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

    // CHA-12487: a tag hash selects a bucket, not a series identity. Force
    // collisions rather than depending on the probability of a natural one.
    // Nine series also catches implementations that merge an overflow bucket
    // after allowing a small fixed number of collisions.
    fn check_colliding_tag_hashes(kind: u8) {
        let map = make_map();
        for round in [1, 10] {
            for series in 0..9 {
                let tags = || vec![("tenant", series.to_string().into())];
                let value = (series + 1) * round;
                match kind {
                    0 => map.record_count("poll_result", &["bloom_filters"], 42, tags, value),
                    1 => {
                        map.record_gauge("poll_result", &["bloom_filters"], 42, tags, value as f64)
                    }
                    _ => map.record_histogram(
                        "poll_result",
                        &["bloom_filters"],
                        42,
                        tags,
                        value as f64,
                    ),
                }
            }
        }

        let mut metrics = map.flush();
        assert_eq!(
            metrics.len(),
            9,
            "each colliding tag set needs its own series"
        );
        metrics.sort_by(|a, b| a.tags.pairs.cmp(&b.tags.pairs));
        for (series, metric) in metrics.iter().enumerate() {
            assert_eq!(metric.namespace, &["bloom_filters"]);
            assert_eq!(
                metric.tags.pairs,
                vec![("tenant", series.to_string().into())]
            );
            let value = (series + 1) as f64;
            match &metric.value {
                FlushedValue::Count(count) => assert_eq!(*count, (value * 11.0) as i64),
                FlushedValue::Gauge(gauge) => assert_eq!(*gauge, value * 10.0),
                FlushedValue::Histogram(sketch) => {
                    assert_eq!(sketch.count(), 2);
                    assert_eq!(sketch.sum(), value * 11.0);
                    assert_eq!(sketch.min(), value);
                    assert_eq!(sketch.max(), value * 10.0);
                }
            }
        }
    }

    #[test]
    fn colliding_tag_hashes_preserve_counts() {
        check_colliding_tag_hashes(0);
    }

    #[test]
    fn colliding_tag_hashes_preserve_gauges() {
        check_colliding_tag_hashes(1);
    }

    #[test]
    fn colliding_tag_hashes_preserve_histogram_samples() {
        check_colliding_tag_hashes(2);
    }

    #[test]
    fn colliding_tag_hashes_preserve_raw_distribution_tags() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(32);
        let map = StripedAggMap::new(200, 0.001, Some(sender));
        for series in 0..9 {
            for round in [1, 10] {
                map.record_histogram(
                    "latency",
                    &["bloom_filters"],
                    42,
                    || vec![("tenant", series.to_string().into())],
                    ((series + 1) * round) as f64,
                );
            }
        }

        for series in 0..9 {
            for round in [1, 10] {
                let point = receiver.try_recv().expect("every observation is delivered");
                assert_eq!(
                    point.tags.pairs,
                    vec![("tenant", series.to_string().into())]
                );
                assert_eq!(point.value, ((series + 1) * round) as f64);
            }
        }
        assert!(receiver.try_recv().is_err());
        assert_eq!(map.dropped_raw_samples(), 0);
        assert!(map.flush().is_empty());
    }

    #[test]
    fn borrowed_matches_keep_cached_tags_without_merging_collisions() {
        let map = make_map();
        for series in 0..9 {
            let tenant = series.to_string();
            let matches = |pairs: &[(&'static str, std::borrow::Cow<'static, str>)]| {
                pairs.len() == 1 && pairs[0].0 == "tenant" && pairs[0].1 == tenant
            };
            let make = || vec![("tenant", tenant.clone().into())];
            map.record_count_matched(MetricName::new("count"), &[], 42, matches, make, 1);
            map.record_gauge_matched(MetricName::new("gauge"), &[], 42, matches, make, 1.0);
            map.record_histogram_matched(MetricName::new("histogram"), &[], 42, matches, make, 1.0);
            let cached = || panic!("borrowed matching does not rebuild cached tags");
            map.record_count_matched(MetricName::new("count"), &[], 42, matches, cached, 2);
            map.record_gauge_matched(MetricName::new("gauge"), &[], 42, matches, cached, 2.0);
            map.record_histogram_matched(
                MetricName::new("histogram"),
                &[],
                42,
                matches,
                cached,
                2.0,
            );
        }
        let flushed = map.flush();
        assert_eq!(flushed.len(), 27);
        for metric in flushed {
            match metric.value {
                FlushedValue::Count(value) => assert_eq!(value, 3),
                FlushedValue::Gauge(value) => assert_eq!(value, 2.0),
                FlushedValue::Histogram(sketch) => {
                    assert_eq!(sketch.count(), 2);
                    assert_eq!(sketch.sum(), 3.0);
                }
            }
        }
    }

    #[test]
    fn metric_names_compare_by_content() {
        let map = make_map();
        let first = Box::leak(String::from("same_name").into_boxed_str());
        let second = Box::leak(String::from("same_name").into_boxed_str());
        assert_ne!(first.as_ptr(), second.as_ptr());
        map.record_count(first, &[], 42, Vec::new, 1);
        map.record_count(second, &[], 42, Vec::new, 99);
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
            let cached = Vec::new;
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
        map.record_count("test_count", &[], 123, || vec![("k", "v".into())], 3);

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
        map.record_gauge("test_gauge", &[], 456, Vec::new, 20.0);

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
        map.record_histogram("test_hist", &[], 789, Vec::new, 43.0);

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
