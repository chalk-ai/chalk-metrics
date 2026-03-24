use parking_lot::Mutex;

use super::sketch::UDDSketch;

/// A mutex-protected histogram slot backed by a UDD Sketch.
///
/// On flush, the sketch is cloned and replaced with a fresh empty sketch,
/// so that the next aggregation period starts clean.
pub struct HistogramSlot {
    sketch: Mutex<UDDSketch>,
    max_buckets: u64,
    initial_error: f64,
}

impl HistogramSlot {
    pub fn new(max_buckets: u64, initial_error: f64) -> Self {
        Self {
            sketch: Mutex::new(UDDSketch::new(max_buckets, initial_error)),
            max_buckets,
            initial_error,
        }
    }

    /// Record a value into the histogram.
    #[inline]
    pub fn record(&self, value: f64) {
        self.sketch.lock().add_value(value);
    }

    /// Flush the histogram: clone the current sketch and replace with a fresh one.
    /// Returns the sketch containing all values recorded since the last flush.
    pub fn flush(&self) -> UDDSketch {
        let mut guard = self.sketch.lock();
        let snapshot = guard.clone();
        *guard = UDDSketch::new(self.max_buckets, self.initial_error);
        snapshot
    }
}

impl Default for HistogramSlot {
    fn default() -> Self {
        Self::new(200, 0.001)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_record_and_flush() {
        let slot = HistogramSlot::default();
        slot.record(1.0);
        slot.record(2.0);
        slot.record(3.0);

        let sketch = slot.flush();
        assert_eq!(sketch.count(), 3);
        assert_eq!(sketch.min(), 1.0);
        assert_eq!(sketch.max(), 3.0);
    }

    #[test]
    fn test_flush_resets() {
        let slot = HistogramSlot::default();
        slot.record(42.0);

        let sketch1 = slot.flush();
        assert_eq!(sketch1.count(), 1);

        let sketch2 = slot.flush();
        assert_eq!(sketch2.count(), 0);
    }

    #[test]
    fn test_concurrent() {
        let slot = Arc::new(HistogramSlot::default());
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let slot = Arc::clone(&slot);
                thread::spawn(move || {
                    for i in 0..1000 {
                        slot.record(i as f64);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let sketch = slot.flush();
        assert_eq!(sketch.count(), 4000);
    }
}
