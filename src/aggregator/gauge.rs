use std::sync::atomic::{AtomicU64, Ordering};

/// A lock-free gauge that stores the last recorded f64 value.
///
/// Stores the f64 as a bit pattern in an `AtomicU64` for lock-free
/// last-writer-wins semantics. On flush, the value persists (gauges
/// represent current state, not deltas).
pub struct GaugeSlot {
    bits: AtomicU64,
}

/// Sentinel bit pattern indicating no value has been recorded.
/// We use a specific NaN pattern that won't occur naturally.
const UNSET: u64 = u64::MAX;

impl GaugeSlot {
    pub fn new() -> Self {
        Self {
            bits: AtomicU64::new(UNSET),
        }
    }

    /// Atomically store a new gauge value (last-writer-wins).
    #[inline]
    pub fn record(&self, value: f64) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }

    /// Read the current gauge value. Returns `None` if no value has been
    /// recorded. The value is **not** reset — gauges persist across flushes.
    #[inline]
    pub fn flush(&self) -> Option<f64> {
        let bits = self.bits.load(Ordering::Relaxed);
        if bits == UNSET {
            None
        } else {
            Some(f64::from_bits(bits))
        }
    }

    /// Returns `true` if a value has been recorded.
    #[allow(dead_code)]
    pub fn has_value(&self) -> bool {
        self.bits.load(Ordering::Relaxed) != UNSET
    }
}

impl Default for GaugeSlot {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_record_single() {
        let slot = GaugeSlot::new();
        slot.record(42.5);
        assert_eq!(slot.flush(), Some(42.5));
    }

    #[test]
    fn test_unset_returns_none() {
        let slot = GaugeSlot::new();
        assert_eq!(slot.flush(), None);
        assert!(!slot.has_value());
    }

    #[test]
    fn test_flush_does_not_reset() {
        let slot = GaugeSlot::new();
        slot.record(10.0);
        assert_eq!(slot.flush(), Some(10.0));
        assert_eq!(slot.flush(), Some(10.0)); // persists
    }

    #[test]
    fn test_last_writer_wins() {
        let slot = GaugeSlot::new();
        slot.record(1.0);
        slot.record(2.0);
        slot.record(3.0);
        assert_eq!(slot.flush(), Some(3.0));
    }

    #[test]
    fn test_negative_and_zero() {
        let slot = GaugeSlot::new();
        slot.record(-5.5);
        assert_eq!(slot.flush(), Some(-5.5));
        slot.record(0.0);
        assert_eq!(slot.flush(), Some(0.0));
    }

    #[test]
    fn test_concurrent() {
        let slot = Arc::new(GaugeSlot::new());
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let slot = Arc::clone(&slot);
                thread::spawn(move || {
                    for j in 0..100 {
                        slot.record((i * 100 + j) as f64);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Final value should be one of the values written by any thread
        let val = slot.flush().unwrap();
        assert!(val >= 0.0 && val < 400.0);
    }
}
