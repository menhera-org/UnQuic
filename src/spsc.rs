//! Fixed-capacity single-producer/single-consumer handoff queue.
//!
//! Construct at startup and move each endpoint to its sole owner. Push and pop
//! allocate nothing, take no locks and make no syscalls. Notify the destination
//! after publishing a batch; it must drain notifications and queues before sleep.

use std::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

/// Number of entries in every worker-pair queue.
pub const CAPACITY: usize = 1024;

#[repr(align(64))]
struct Cursor(AtomicUsize);

struct Shared<T> {
    head: Cursor,
    tail: Cursor,
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
}

// SAFETY: sole producer/consumer handles transfer each initialized slot with
// release/acquire cursors. Their mutable methods prevent concurrent same-side use.
unsafe impl<T: Send> Sync for Shared<T> {}

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        let mut tail = *self.tail.0.get_mut();
        let head = *self.head.0.get_mut();
        while tail != head {
            // SAFETY: the last Arc owns all remaining initialized entries; neither
            // endpoint can access them now. Consumed entries are outside this range.
            unsafe {
                self.slots[tail & (CAPACITY - 1)]
                    .get_mut()
                    .assume_init_drop()
            };
            tail = tail.wrapping_add(1);
        }
    }
}

/// Sole producer endpoint. Intentionally not Clone.
pub struct Producer<T> {
    shared: Arc<Shared<T>>,
    head: usize,
    cached_tail: usize,
}

/// Sole consumer endpoint. Intentionally not Clone.
pub struct Consumer<T> {
    shared: Arc<Shared<T>>,
    tail: usize,
    cached_head: usize,
}

/// Allocates one ring and returns its two exclusively owned endpoints.
///
/// A dropped endpoint does not discard the other owner's data. The final owner
/// drops any remaining entries. Shutdown is signaled separately by the runtime.
pub fn channel<T: Send>() -> (Producer<T>, Consumer<T>) {
    let shared = Arc::new(Shared {
        head: Cursor(AtomicUsize::new(0)),
        tail: Cursor(AtomicUsize::new(0)),
        slots: (0..CAPACITY)
            .map(|_| UnsafeCell::new(MaybeUninit::uninit()))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    });
    (
        Producer {
            shared: Arc::clone(&shared),
            head: 0,
            cached_tail: 0,
        },
        Consumer {
            shared,
            tail: 0,
            cached_head: 0,
        },
    )
}

impl<T> Producer<T> {
    /// Checks space before preparing an item; only this producer can consume it.
    #[inline]
    pub fn is_full(&mut self) -> bool {
        if self.head.wrapping_sub(self.cached_tail) == CAPACITY {
            self.cached_tail = self.shared.tail.0.load(Ordering::Acquire);
        }
        self.head.wrapping_sub(self.cached_tail) == CAPACITY
    }

    /// Publishes an item, or returns it unchanged when the ring is full.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        if self.is_full() {
            return Err(value);
        }
        // SAFETY: this endpoint exclusively owns the free slot. An acquired tail
        // proves the consumer finished reading before this slot can be reused.
        unsafe {
            (*self.shared.slots[self.head & (CAPACITY - 1)].get()).write(value);
        }
        self.head = self.head.wrapping_add(1);
        self.shared.head.0.store(self.head, Ordering::Release);
        Ok(())
    }
}

impl<T> Consumer<T> {
    /// Removes the oldest published item, or returns None when empty.
    #[inline]
    pub fn try_pop(&mut self) -> Option<T> {
        if self.tail == self.cached_head {
            self.cached_head = self.shared.head.0.load(Ordering::Acquire);
            if self.tail == self.cached_head {
                return None;
            }
        }
        // SAFETY: the acquired head proves this slot was initialized. Only this
        // endpoint reads it, and the tail is released after moving its value out.
        let value =
            unsafe { (*self.shared.slots[self.tail & (CAPACITY - 1)].get()).assume_init_read() };
        self.tail = self.tail.wrapping_add(1);
        self.shared.tail.0.store(self.tail, Ordering::Release);
        Some(value)
    }

    /// Reports whether no published item is waiting, refreshing the cached head.
    #[inline]
    pub fn is_empty(&mut self) -> bool {
        if self.tail == self.cached_head {
            self.cached_head = self.shared.head.0.load(Ordering::Acquire);
        }
        self.tail == self.cached_head
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn full_ring_returns_ownership_and_wraparound_preserves_order() {
        let (mut tx, mut rx) = channel();
        let start = usize::MAX - 8;
        tx.head = start;
        tx.cached_tail = start;
        rx.tail = start;
        rx.cached_head = start;
        tx.shared.head.0.store(start, Ordering::Relaxed);
        tx.shared.tail.0.store(start, Ordering::Relaxed);
        for round in 0..4 {
            assert!(rx.is_empty());
            for i in 0..CAPACITY {
                assert_eq!(tx.try_push(round * CAPACITY + i), Ok(()));
                assert!(!rx.is_empty());
            }
            assert!(tx.is_full());
            assert_eq!(tx.try_push(usize::MAX), Err(usize::MAX));
            for i in 0..CAPACITY {
                assert!(!rx.is_empty());
                assert_eq!(rx.try_pop(), Some(round * CAPACITY + i));
            }
            assert!(rx.is_empty());
            assert_eq!(rx.try_pop(), None);
            assert!(!tx.is_full());
        }
    }

    #[test]
    fn remaining_and_consumed_values_are_dropped_exactly_once() {
        struct Value(Arc<AtomicU64>);
        impl Drop for Value {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let drops = Arc::new(AtomicU64::new(0));
        let (mut tx, mut rx) = channel();
        for _ in 0..CAPACITY {
            assert!(tx.try_push(Value(Arc::clone(&drops))).is_ok());
        }
        drop(tx.try_push(Value(Arc::clone(&drops))).err().unwrap());
        for _ in 0..10 {
            drop(rx.try_pop().unwrap());
        }
        assert_eq!(drops.load(Ordering::Relaxed), 11);
        drop(rx);
        assert_eq!(drops.load(Ordering::Relaxed), 11);
        drop(tx);
        assert_eq!(drops.load(Ordering::Relaxed), CAPACITY as u64 + 1);
    }

    #[test]
    fn dropping_producer_keeps_published_items_available() {
        let (mut tx, mut rx) = channel();
        tx.try_push(String::from("owned")).unwrap();
        drop(tx);
        assert_eq!(rx.try_pop().as_deref(), Some("owned"));
        assert_eq!(rx.try_pop(), None);
    }

    #[test]
    fn two_threads_transfer_items_in_order() {
        let (mut tx, mut rx) = channel();
        let count = if cfg!(miri) { 128 } else { 1_000_000 };
        let producer = std::thread::spawn(move || {
            for n in 0..count {
                while tx.try_push(n).is_err() {
                    std::hint::spin_loop();
                }
            }
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        for expected in 0..count {
            let mut spins = 0;
            let value = loop {
                if let Some(value) = rx.try_pop() {
                    break value;
                }
                spins += 1;
                if spins % 65536 == 0 {
                    assert!(std::time::Instant::now() < deadline);
                }
                std::hint::spin_loop();
            };
            assert_eq!(value, expected);
        }
        producer.join().unwrap();
        assert_eq!(rx.try_pop(), None);
    }
}
