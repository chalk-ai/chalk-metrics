// UDD Sketch implementation for approximate quantile estimation.
// Adapted from Timescale's implementation (Apache 2.0 license).
// See: https://github.com/timescale/timescaledb-toolkit/blob/main/crates/udd-sketch/src/lib.rs
// Paper: https://arxiv.org/pdf/2004.08604.pdf

use std::collections::HashMap;

/// Bucket key for the UDD Sketch. Values are mapped to logarithmic buckets;
/// negative values, zero, and positive values are tracked separately.
#[derive(Hash, PartialEq, Eq, Copy, Clone, Debug)]
pub enum SketchHashKey {
    Negative(i64),
    Zero,
    Positive(i64),
    Invalid,
}

impl SketchHashKey {
    pub fn is_valid(&self) -> bool {
        !matches!(self, Self::Invalid)
    }

    /// Compute the key after one compaction round. Odd buckets merge with
    /// the bucket after them.
    fn compact_key(&self) -> SketchHashKey {
        use SketchHashKey::*;
        match *self {
            Negative(i64::MAX) | Positive(i64::MAX) => *self,
            Negative(x) => Negative(if x > 0 { x + 1 } else { x } / 2),
            Positive(x) => Positive(if x > 0 { x + 1 } else { x } / 2),
            x => x,
        }
    }
}

impl PartialOrd for SketchHashKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        use self::SketchHashKey::*;
        use std::cmp::Ordering::*;
        match (self, other) {
            (Invalid, Invalid) => Equal,
            (Invalid, _) => Greater,
            (_, Invalid) => Less,
            (Zero, Zero) => Equal,
            (Positive(a), Positive(b)) => a.cmp(b),
            (Negative(a), Negative(b)) => a.cmp(b).reverse(),
            (_, Positive(_)) => Less,
            (Positive(_), _) => Greater,
            (_, Negative(_)) => Greater,
            (Negative(_), _) => Less,
        }
        .into()
    }
}

#[derive(Debug, Clone, PartialEq)]
struct SketchHashEntry {
    count: u64,
    next: SketchHashKey,
}

/// Ordered hash map of `SketchHashKey -> count` with linked-list traversal
/// in increasing key order.
#[derive(Debug, Clone, PartialEq)]
struct SketchHashMap {
    map: HashMap<SketchHashKey, SketchHashEntry>,
    head: SketchHashKey,
}

impl std::ops::Index<SketchHashKey> for SketchHashMap {
    type Output = u64;
    fn index(&self, id: SketchHashKey) -> &Self::Output {
        &self.map[&id].count
    }
}

/// Iterator over `(SketchHashKey, count)` pairs in increasing key order.
pub struct SketchHashIterator<'a> {
    container: &'a SketchHashMap,
    next_key: SketchHashKey,
}

impl Iterator for SketchHashIterator<'_> {
    type Item = (SketchHashKey, u64);
    fn next(&mut self) -> Option<(SketchHashKey, u64)> {
        if self.next_key == SketchHashKey::Invalid {
            None
        } else {
            let key = self.next_key;
            self.next_key = self.container.map[&self.next_key].next;
            Some((key, self.container[key]))
        }
    }
}

impl SketchHashMap {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            head: SketchHashKey::Invalid,
        }
    }

    fn increment(&mut self, key: SketchHashKey) {
        self.entry(key).count += 1;
    }

    fn iter(&self) -> SketchHashIterator<'_> {
        SketchHashIterator {
            container: self,
            next_key: self.head,
        }
    }

    fn entry(&mut self, key: SketchHashKey) -> &mut SketchHashEntry {
        let mut next = self.head;
        if !self.map.contains_key(&key) {
            if key < self.head {
                self.head = key;
            } else {
                let mut prev = SketchHashKey::Invalid;
                while key > next {
                    prev = next;
                    next = self.map[&next].next;
                }
                self.map.get_mut(&prev).expect("Invalid key found").next = key;
            }
        }
        self.map
            .entry(key)
            .or_insert(SketchHashEntry { count: 0, next })
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn compact(&mut self) {
        let mut target = self.head;
        let old_map = std::mem::take(&mut self.map);
        self.head = self.head.compact_key();

        while target != SketchHashKey::Invalid {
            let old_entry = &old_map[&target];
            let new_key = target.compact_key();
            let new_next = if old_entry.next.compact_key() == new_key {
                old_map[&old_entry.next].next.compact_key()
            } else {
                old_entry.next.compact_key()
            };
            self.map
                .entry(new_key)
                .or_insert(SketchHashEntry {
                    count: 0,
                    next: new_next,
                })
                .count += old_entry.count;
            target = old_map[&target].next;
        }
    }
}

/// Approximate quantile sketch with bounded relative error.
///
/// Uses logarithmic bucketing with configurable maximum buckets and error bound.
/// When the number of buckets exceeds `max_buckets`, adjacent buckets are merged
/// (compacted), increasing the error bound.
///
/// Space complexity: O(max_buckets). Default: 200 buckets, 0.1% error.
#[derive(Clone, Debug, PartialEq)]
pub struct UDDSketch {
    buckets: SketchHashMap,
    alpha: f64,
    gamma: f64,
    compactions: u32,
    max_buckets: u64,
    num_values: u64,
    values_sum: f64,
    min: f64,
    max: f64,
    zero_count: u64,
}

impl UDDSketch {
    /// Create a new sketch with the given maximum bucket count and initial error bound.
    ///
    /// # Panics
    /// Panics if `initial_error` is not in `[1e-12, 1.0)`.
    pub fn new(max_buckets: u64, initial_error: f64) -> Self {
        assert!((1e-12..1.0).contains(&initial_error));
        UDDSketch {
            buckets: SketchHashMap::new(),
            alpha: initial_error,
            gamma: (1.0 + initial_error) / (1.0 - initial_error),
            compactions: 0,
            max_buckets,
            num_values: 0,
            values_sum: 0.0,
            min: f64::MAX,
            max: f64::MIN,
            zero_count: 0,
        }
    }

    /// Add a value to the sketch. NaN values are silently skipped.
    pub fn add_value(&mut self, value: f64) {
        if value.is_nan() {
            return;
        }

        self.buckets.increment(self.key(value));

        while self.buckets.len() > self.max_buckets as usize {
            self.compact_buckets();
        }

        self.num_values += 1;
        self.values_sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        if value == 0.0 {
            self.zero_count += 1;
        }
    }

    /// Merge another sketch into this one. Both sketches must have the same
    /// `max_buckets` setting.
    pub fn merge_sketch(&mut self, other: &UDDSketch) {
        assert_eq!(self.max_buckets, other.max_buckets);

        if other.num_values == 0 {
            return;
        }
        if self.num_values == 0 {
            *self = other.clone();
            return;
        }

        let mut other = other.clone();

        while self.compactions > other.compactions {
            other.compact_buckets();
        }
        while other.compactions > self.compactions {
            self.compact_buckets();
        }

        for (key, value) in other.buckets.iter() {
            self.buckets.entry(key).count += value;
        }

        while self.buckets.len() > self.max_buckets as usize {
            self.compact_buckets();
        }

        self.num_values += other.num_values;
        self.values_sum += other.values_sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
        self.zero_count += other.zero_count;
    }

    /// Compact adjacent buckets, reducing bucket count and increasing error.
    pub fn compact_buckets(&mut self) {
        self.buckets.compact();
        self.compactions += 1;
        self.gamma *= self.gamma;
        self.alpha = 2.0 * self.alpha / (1.0 + self.alpha.powi(2));
    }

    /// Estimate the value at the given quantile (0.0 to 1.0).
    pub fn estimate_quantile(&self, quantile: f64) -> f64 {
        assert!((0.0..=1.0).contains(&quantile));

        let mut remaining = (self.num_values as f64 * quantile) as u64 + 1;
        if remaining >= self.num_values {
            return self
                .buckets
                .iter()
                .last()
                .map(|(key, _)| self.bucket_to_value(key))
                .unwrap_or(0.0);
        }

        for (key, count) in self.buckets.iter() {
            if remaining <= count {
                return self.bucket_to_value(key);
            }
            remaining -= count;
        }
        unreachable!();
    }

    #[inline]
    pub fn mean(&self) -> f64 {
        if self.num_values == 0 {
            0.0
        } else {
            self.values_sum / self.num_values as f64
        }
    }

    #[inline]
    pub fn sum(&self) -> f64 {
        self.values_sum
    }

    #[inline]
    pub fn count(&self) -> u64 {
        self.num_values
    }

    #[inline]
    pub fn min(&self) -> f64 {
        self.min
    }

    #[inline]
    pub fn max(&self) -> f64 {
        self.max
    }

    #[inline]
    pub fn zero_count(&self) -> u64 {
        self.zero_count
    }

    #[inline]
    pub fn max_error(&self) -> f64 {
        self.alpha
    }

    #[inline]
    pub fn current_buckets_count(&self) -> usize {
        self.buckets.map.len()
    }

    /// Compute the bucket key for a given value. Public for use by exporters.
    pub fn key_for_value(&self, value: f64) -> SketchHashKey {
        self.key(value)
    }

    /// Iterator over `(SketchHashKey, count)` pairs in increasing key order.
    pub fn bucket_iter(&self) -> SketchHashIterator<'_> {
        self.buckets.iter()
    }

    /// Estimate the fraction of values <= the given value (CDF).
    /// Returns a value in `[0.0, 1.0]`.
    pub fn estimate_quantile_at_value(&self, value: f64) -> f64 {
        if self.num_values == 0 {
            return 0.0;
        }
        let target = self.key(value);
        let mut count = 0.0;
        for (key, bucket_count) in self.buckets.iter() {
            if target > key {
                count += bucket_count as f64;
            } else {
                if target == key {
                    count += bucket_count as f64 / 2.0;
                }
                return count / self.num_values as f64;
            }
        }
        1.0
    }

    fn key(&self, value: f64) -> SketchHashKey {
        let negative = value < 0.0;
        let value = value.abs();
        if value == 0.0 {
            SketchHashKey::Zero
        } else if negative {
            SketchHashKey::Negative(value.log(self.gamma).ceil() as i64)
        } else {
            SketchHashKey::Positive(value.log(self.gamma).ceil() as i64)
        }
    }

    fn bucket_to_value(&self, bucket: SketchHashKey) -> f64 {
        match bucket {
            SketchHashKey::Zero => 0.0,
            SketchHashKey::Positive(i) => self.gamma.powf(i as f64 - 1.0) * (1.0 + self.alpha),
            SketchHashKey::Negative(i) => -(self.gamma.powf(i as f64 - 1.0) * (1.0 + self.alpha)),
            SketchHashKey::Invalid => panic!("Cannot convert invalid bucket to value"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_single_value() {
        let mut sketch = UDDSketch::new(200, 0.001);
        sketch.add_value(42.0);
        assert_eq!(sketch.count(), 1);
        assert_eq!(sketch.sum(), 42.0);
        assert_eq!(sketch.min(), 42.0);
        assert_eq!(sketch.max(), 42.0);
        assert_eq!(sketch.mean(), 42.0);
    }

    #[test]
    fn test_add_multiple_values() {
        let mut sketch = UDDSketch::new(200, 0.001);
        for i in 1..=100 {
            sketch.add_value(i as f64);
        }
        assert_eq!(sketch.count(), 100);
        assert_eq!(sketch.min(), 1.0);
        assert_eq!(sketch.max(), 100.0);
        assert!((sketch.mean() - 50.5).abs() < 1e-10);
    }

    #[test]
    fn test_nan_skipped() {
        let mut sketch = UDDSketch::new(200, 0.001);
        sketch.add_value(1.0);
        sketch.add_value(f64::NAN);
        sketch.add_value(2.0);
        assert_eq!(sketch.count(), 2);
        assert_eq!(sketch.sum(), 3.0);
    }

    #[test]
    fn test_zero_handling() {
        let mut sketch = UDDSketch::new(200, 0.001);
        sketch.add_value(0.0);
        sketch.add_value(0.0);
        sketch.add_value(1.0);
        assert_eq!(sketch.zero_count(), 2);
        assert_eq!(sketch.count(), 3);
    }

    #[test]
    fn test_quantile_estimation() {
        let mut sketch = UDDSketch::new(200, 0.01);
        for v in 1..=10000 {
            sketch.add_value(v as f64);
        }

        let p50 = sketch.estimate_quantile(0.5);
        let p99 = sketch.estimate_quantile(0.99);

        // p50 should be close to 5000
        assert!(
            (p50 - 5000.0).abs() / 5000.0 < 0.02,
            "p50 = {p50}, expected ~5000"
        );
        // p99 should be close to 9900 (within sketch error bounds)
        assert!(
            (p99 - 9900.0).abs() / 9900.0 < 0.05,
            "p99 = {p99}, expected ~9900"
        );
    }

    #[test]
    fn test_quantile_boundaries() {
        let mut sketch = UDDSketch::new(200, 0.01);
        for v in 1..=100 {
            sketch.add_value(v as f64);
        }
        // quantile 0.0 should return roughly the minimum
        let q0 = sketch.estimate_quantile(0.0);
        assert!(q0 <= 2.0, "q0 = {q0}");
        // quantile 1.0 should return roughly the maximum
        let q1 = sketch.estimate_quantile(1.0);
        assert!(q1 >= 99.0, "q1 = {q1}");
    }

    #[test]
    fn test_compact() {
        let mut sketch = UDDSketch::new(20, 0.1);
        // Add enough distinct values to trigger compaction
        for i in 0..30 {
            sketch.add_value(1000.0 * 1.23_f64.powi(i));
        }
        assert_eq!(sketch.count(), 30);
        assert!(sketch.current_buckets_count() <= 20);
    }

    #[test]
    fn test_merge() {
        let mut s1 = UDDSketch::new(200, 0.001);
        let mut s2 = UDDSketch::new(200, 0.001);

        for v in 1..=50 {
            s1.add_value(v as f64);
        }
        for v in 51..=100 {
            s2.add_value(v as f64);
        }

        s1.merge_sketch(&s2);
        assert_eq!(s1.count(), 100);
        assert_eq!(s1.min(), 1.0);
        assert_eq!(s1.max(), 100.0);
        assert!((s1.mean() - 50.5).abs() < 1e-10);
    }

    #[test]
    fn test_merge_empty() {
        let mut s1 = UDDSketch::new(200, 0.001);
        let s2 = UDDSketch::new(200, 0.001);

        s1.add_value(42.0);
        s1.merge_sketch(&s2); // merge empty into non-empty
        assert_eq!(s1.count(), 1);

        let mut s3 = UDDSketch::new(200, 0.001);
        let s4 = UDDSketch::new(200, 0.001);
        s3.add_value(10.0);
        let mut empty = UDDSketch::new(200, 0.001);
        empty.merge_sketch(&s3); // merge non-empty into empty
        assert_eq!(empty.count(), 1);

        let _ = s4;
    }

    #[test]
    fn test_negative_values() {
        let mut sketch = UDDSketch::new(200, 0.01);
        for v in -100..=100 {
            sketch.add_value(v as f64);
        }
        assert_eq!(sketch.count(), 201);
        assert_eq!(sketch.min(), -100.0);
        assert_eq!(sketch.max(), 100.0);
        assert_eq!(sketch.zero_count(), 1);
    }

    #[test]
    fn test_empty_sketch() {
        let sketch = UDDSketch::new(200, 0.001);
        assert_eq!(sketch.count(), 0);
        assert_eq!(sketch.mean(), 0.0);
        assert_eq!(sketch.sum(), 0.0);
        assert_eq!(sketch.zero_count(), 0);
    }
}
