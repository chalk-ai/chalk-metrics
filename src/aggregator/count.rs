use std::sync::atomic::{AtomicI64, Ordering};

/// A lock-free counter that accumulates deltas between flushes.
///
/// Uses `AtomicI64` with relaxed ordering for maximum throughput.
/// On flush, the accumulated value is atomically swapped to zero.
pub struct CountSlot {
    value: AtomicI64,
}

impl CountSlot {
    pub fn new() -> Self {
        Self {
            value: AtomicI64::new(0),
        }
    }

    /// Atomically add `delta` to the counter.
    #[inline]
    pub fn record(&self, delta: i64) {
        self.value.fetch_add(delta, Ordering::Relaxed);
    }

    /// Atomically read and reset the counter, returning the accumulated value
    /// since the last flush.
    #[inline]
    pub fn flush(&self) -> i64 {
        self.value.swap(0, Ordering::Relaxed)
    }
}

impl Default for CountSlot {
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
        let slot = CountSlot::new();
        slot.record(5);
        assert_eq!(slot.flush(), 5);
    }

    #[test]
    fn test_record_multiple_increments() {
        let slot = CountSlot::new();
        slot.record(1);
        slot.record(2);
        slot.record(3);
        assert_eq!(slot.flush(), 6);
    }

    #[test]
    fn test_flush_resets() {
        let slot = CountSlot::new();
        slot.record(10);
        assert_eq!(slot.flush(), 10);
        assert_eq!(slot.flush(), 0);
    }

    #[test]
    fn test_negative_delta() {
        let slot = CountSlot::new();
        slot.record(10);
        slot.record(-3);
        assert_eq!(slot.flush(), 7);
    }

    #[test]
    fn test_concurrent() {
        let slot = Arc::new(CountSlot::new());
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let slot = Arc::clone(&slot);
                thread::spawn(move || {
                    for _ in 0..1000 {
                        slot.record(1);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(slot.flush(), 4000);
    }
}
