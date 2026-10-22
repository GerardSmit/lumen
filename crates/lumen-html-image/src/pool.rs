use super::ImageError;
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};

const MAX_BUFFERS: usize = 8;
const MAX_POOLED_BYTES: usize = 32 << 20;
const MAX_TOTAL_POOLED_BYTES: usize = 48 << 20;
const MIN_POOLED_BYTES: usize = 64 << 10;

/// Capacity held by every thread's list together.
static POOLED_TOTAL: AtomicUsize = AtomicUsize::new(0);

/// A thread's buffers; the capacity they held leaves the global total when the thread exits.
struct Buffers(Vec<Vec<u8>>);

impl Drop for Buffers {
    fn drop(&mut self) {
        let held: usize = self.0.iter().map(Vec::capacity).sum();
        POOLED_TOTAL.fetch_sub(held, Ordering::Relaxed);
    }
}

thread_local! {
    static BUFFERS: RefCell<Buffers> = const { RefCell::new(Buffers(Vec::new())) };
}

/// Per-thread scratch buffers for surfaces and planes that live within one raster pass.
/// Raster threads are long-lived, so a returned buffer serves the next frame without the
/// allocator handing its pages back to the OS.
fn take(len: usize) -> Result<Vec<u8>, ImageError> {
    let reused = BUFFERS.with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        let buffers = &mut buffers.0;
        let best = buffers
            .iter()
            .enumerate()
            .filter(|(_, buffer)| buffer.capacity() >= len && buffer.capacity() / 2 <= len)
            .min_by_key(|(_, buffer)| buffer.capacity())
            .map(|(index, _)| index)?;
        Some(buffers.swap_remove(best))
    });
    if let Some(mut buffer) = reused {
        POOLED_TOTAL.fetch_sub(buffer.capacity(), Ordering::Relaxed);
        buffer.clear();
        return Ok(buffer);
    }
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(len).map_err(|_| ImageError::TooLarge)?;
    Ok(buffer)
}

/// An empty buffer with room for `len` bytes.
pub(super) fn take_empty(len: usize) -> Result<Vec<u8>, ImageError> {
    take(len)
}

/// A buffer of `len` zero bytes.
pub(super) fn take_zeroed(len: usize) -> Result<Vec<u8>, ImageError> {
    let mut buffer = take(len)?;
    buffer.resize(len, 0);
    Ok(buffer)
}

pub(super) fn recycle(buffer: Vec<u8>) {
    let capacity = buffer.capacity();
    if !(MIN_POOLED_BYTES..=MAX_POOLED_BYTES).contains(&capacity) {
        return;
    }
    BUFFERS.with(|buffers| {
        let mut buffers = buffers.borrow_mut();
        let buffers = &mut buffers.0;
        let mut held: usize = buffers.iter().map(Vec::capacity).sum();
        while buffers.len() >= MAX_BUFFERS || held + capacity > MAX_POOLED_BYTES {
            let Some(smallest) = buffers.iter().enumerate().min_by_key(|(_, buffer)| buffer.capacity()).map(|(index, _)| index) else {
                break;
            };
            let dropped = buffers.swap_remove(smallest).capacity();
            held -= dropped;
            POOLED_TOTAL.fetch_sub(dropped, Ordering::Relaxed);
        }
        if held + capacity <= MAX_POOLED_BYTES && reserve_global(capacity) {
            buffers.push(buffer);
        }
    });
}

fn reserve_global(capacity: usize) -> bool {
    POOLED_TOTAL
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
            (total + capacity <= MAX_TOTAL_POOLED_BYTES).then_some(total + capacity)
        })
        .is_ok()
}

/// Returns the pixel storage of layer images no other holder references.
pub(super) fn recycle_layers(list: std::borrow::Cow<'_, lumen_html::paint::DisplayList>) {
    if let std::borrow::Cow::Owned(list) = list {
        for command in list.0 {
            if let lumen_html::paint::Command::Image { image, .. } = command {
                if let Ok(data) = std::sync::Arc::try_unwrap(image) {
                    recycle(data.pixels);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn pooled_bytes_across_threads_stay_under_the_global_cap() {
        let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let size = 16 << 20;
        let handles: Vec<_> = (0..6)
            .map(|_| {
                std::thread::spawn(move || {
                    let buffer = take_zeroed(size).unwrap();
                    recycle(buffer);
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(POOLED_TOTAL.load(Ordering::Relaxed) <= MAX_TOTAL_POOLED_BYTES);
    }

    #[test]
    fn exited_threads_do_not_use_up_the_global_cap() {
        let _serial = SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let size = 16 << 20;
        for _ in 0..8 {
            let pooled = std::thread::spawn(move || {
                recycle(vec![0u8; size]);
                BUFFERS.with(|buffers| buffers.borrow().0.len())
            })
            .join()
            .unwrap();
            assert_eq!(pooled, 1);
        }
    }
}
