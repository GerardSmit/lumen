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
    /// Charges additional actual capacity through the same reservation authority.
    pub fn grow_to(&mut self, bytes: usize) -> bool {
        let Some(additional) = bytes.checked_sub(self.bytes) else { return false; };
        let Some(mut extension) = self.budget.reserve(additional) else { return false; };
        extension.bytes = 0;
        self.bytes = bytes;
        true
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

/// An immutable owned string charged for its actual allocation capacity.
/// No Clone or mutable buffer escape can retain an unaccounted allocation.
pub struct BudgetedString {
    value:alloc::string::String,
    _reservation:ByteLease,
}
impl BudgetedString {
    pub fn copy(source:&str,budget:&Arc<ByteBudget>)->Result<Self,crate::limits::size::TooLarge>{
        use crate::limits::size::TooLarge;
        let mut reservation=budget.reserve(source.len()).ok_or(TooLarge)?;
        let mut value=alloc::string::String::new();
        value.try_reserve_exact(source.len()).map_err(|_|TooLarge)?;
        if value.capacity()>source.len() && !reservation.grow_to(value.capacity()){return Err(TooLarge);}
        value.push_str(source);
        Ok(Self{value,_reservation:reservation})
    }
    pub fn join<'a,I>(parts:I,separator:&str,budget:&Arc<ByteBudget>)->Result<Self,crate::limits::size::TooLarge>
        where I:Iterator<Item=&'a str>+Clone {
        use crate::limits::size::TooLarge;
        let mut bytes=0usize;let mut has_previous=false;
        for part in parts.clone(){
            if has_previous{bytes=bytes.checked_add(separator.len()).ok_or(TooLarge)?;}
            bytes=bytes.checked_add(part.len()).ok_or(TooLarge)?;has_previous=true;
        }
        let mut reservation=budget.reserve(bytes).ok_or(TooLarge)?;
        let mut value=alloc::string::String::new();
        value.try_reserve_exact(bytes).map_err(|_|TooLarge)?;
        if value.capacity()>bytes && !reservation.grow_to(value.capacity()){return Err(TooLarge);}
        for(index,part)in parts.enumerate(){if index!=0{value.push_str(separator);}value.push_str(part);}
        Ok(Self{value,_reservation:reservation})
    }
    pub fn as_str(&self)->&str{&self.value}
    pub fn allocated_bytes(&self)->usize{self.value.capacity()}
}

/// Fallible capped vector growth charged to an existing owned-resource budget.
/// Old and replacement buffers remain charged together until reallocation
/// completes. Payload reservations stay with their real vector lifetime.
pub struct BudgetedVec<T> {
    values:alloc::vec::Vec<T>,
    reservation:Option<ByteLease>,
    budget:Arc<ByteBudget>,
    max_len:usize,
}
impl<T> BudgetedVec<T> {
    pub fn new(budget:Arc<ByteBudget>,max_len:usize)->Self{
        Self{values:alloc::vec::Vec::new(),reservation:None,budget,max_len}
    }
    pub fn as_slice(&self)->&[T]{&self.values}
    pub fn as_mut_slice(&mut self)->&mut [T]{&mut self.values}
    pub fn len(&self)->usize{self.values.len()}
    pub fn is_empty(&self)->bool{self.values.is_empty()}
    pub fn capacity(&self)->usize{self.values.capacity()}
    pub fn pop(&mut self)->Option<T>{self.values.pop()}
    pub fn push(&mut self,value:T)->Result<(),crate::limits::size::TooLarge>{
        use crate::limits::size::{TooLarge,sum,repeat};
        let needed=sum(self.values.len(),1,self.max_len)?;
        if needed>self.values.capacity(){
            let capacity=self.values.capacity().saturating_mul(2).max(4).min(self.max_len).max(needed);
            let bytes=repeat(capacity,core::mem::size_of::<T>(),usize::MAX)?;
            let mut reservation=self.budget.reserve(bytes).ok_or(TooLarge)?;
            self.values.try_reserve_exact(capacity-self.values.len()).map_err(|_|TooLarge)?;
            let actual=repeat(self.values.capacity(),core::mem::size_of::<T>(),usize::MAX)?;
            if actual>bytes && !reservation.grow_to(actual) {
                // An allocator may provide more than requested. Reject and
                // reclaim that completed buffer rather than retaining uncharged capacity.
                self.values=alloc::vec::Vec::new();
                self.reservation=None;
                return Err(TooLarge);
            }
            self.reservation=Some(reservation);
        }
        self.values.push(value);Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_owned_vector_admits_reallocation_peak_and_reclaims_with_payload(){
        let budget=ByteBudget::new(64);
        let mut values=BudgetedVec::new(budget.clone(),8);
        for value in 0u64..4 {values.push(value).unwrap();}
        assert_eq!(budget.reserved(),32);
        assert!(values.push(4).is_err(),"old 32-byte and replacement 64-byte buffers exceed the shared budget");
        assert_eq!(values.as_slice(),&[0,1,2,3]);
        assert_eq!(budget.reserved(),32);
        assert_eq!(values.pop(),Some(3));
        assert_eq!(values.as_slice(),&[0,1,2]);
        assert_eq!(budget.reserved(),32,"popping releases a value, not its retained buffer capacity");
        drop(values);assert_eq!(budget.reserved(),0);
    }
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
