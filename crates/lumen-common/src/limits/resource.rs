//! Actual owned-resource reservations, independent of allocator/RSS counters.
//! A reservation moves with its payload and releases only at real reclamation.
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

pub struct ByteBudget {
    limit: usize,
    reserved: AtomicUsize,
}
impl ByteBudget {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit,
            reserved: AtomicUsize::new(0),
        })
    }
    pub fn reserved(&self) -> usize {
        self.reserved.load(Ordering::Acquire)
    }
    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn reserve(self: &Arc<Self>, bytes: usize) -> Option<ByteLease> {
        let mut current = self.reserved.load(Ordering::Relaxed);
        loop {
            let next = current
                .checked_add(bytes)
                .filter(|next| *next <= self.limit)?;
            match self.reserved.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Some(ByteLease {
                        budget: self.clone(),
                        bytes,
                    })
                }
                Err(actual) => current = actual,
            }
        }
    }
}

pub struct ByteLease {
    budget: Arc<ByteBudget>,
    bytes: usize,
}
impl ByteLease {
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    /// Replace an admitted maximum with the actual completed payload size.
    pub fn shrink_to(&mut self, bytes: usize) -> bool {
        let Some(released) = self.bytes.checked_sub(bytes) else {
            return false;
        };
        self.budget.reserved.fetch_sub(released, Ordering::AcqRel);
        self.bytes = bytes;
        true
    }
}
impl Drop for ByteLease {
    fn drop(&mut self) {
        self.budget.reserved.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_resource_byte_reservation_follows_payload_and_real_release() {
        let budget = ByteBudget::new(32);
        let mut payload = budget.reserve(24).expect("initial payload reservation");
        assert!(budget.reserve(9).is_none());
        assert!(!payload.shrink_to(25));
        assert!(payload.shrink_to(12));
        let retained = budget.reserve(20).expect("actual remaining bytes");
        assert_eq!(budget.reserved(), 32);
        drop(payload);
        assert_eq!(budget.reserved(), 20);
        assert!(budget.reserve(usize::MAX).is_none());
        drop(retained);
        assert_eq!(budget.reserved(), 0);
    }
}
