//! Immutable image snapshots for allocation-free, lock-free fault PC lookup.
use super::NativeLocation;
use lumen_common::aot::native_data::FunctionEntry;
use std::sync::{
    atomic::{AtomicPtr, AtomicUsize, Ordering},
    Arc, Mutex,
};

struct Image {
    base: usize,
    len: usize,
    hash: [u8; 32],
    functions: Vec<FunctionEntry>,
}

struct Snapshot {
    images: Vec<Arc<Image>>,
}
struct Writer {
    images: Vec<Arc<Image>>,
    retired: Vec<Box<Snapshot>>,
}

static SNAPSHOT: AtomicPtr<Snapshot> = AtomicPtr::new(std::ptr::null_mut());
static READERS: AtomicUsize = AtomicUsize::new(0);
static WRITER: Mutex<Writer> = Mutex::new(Writer {
    images: Vec::new(),
    retired: Vec::new(),
});

fn publish(writer: &mut Writer) {
    let next = Box::into_raw(Box::new(Snapshot {
        images: writer.images.clone(),
    }));
    let old = SNAPSHOT.swap(next, Ordering::SeqCst);
    if !old.is_null() {
        writer.retired.push(unsafe { Box::from_raw(old) });
    }
    // The same sequential ordering covers reader admission and publication.
    // Reclamation runs only on normal load/drop paths, never during a fault.
    if READERS.load(Ordering::SeqCst) == 0 {
        writer.retired.clear();
    }
}

pub(super) struct Registration(Arc<Image>);

impl Registration {
    pub(super) fn new(
        base: usize,
        len: usize,
        hash: [u8; 32],
        functions: &[FunctionEntry],
    ) -> Self {
        let image = Arc::new(Image {
            base,
            len,
            hash,
            functions: functions.to_vec(),
        });
        let mut writer = WRITER.lock().unwrap_or_else(|poison| poison.into_inner());
        writer.images.push(image.clone());
        publish(&mut writer);
        Self(image)
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut writer = WRITER.lock().unwrap_or_else(|poison| poison.into_inner());
        writer.images.retain(|image| !Arc::ptr_eq(image, &self.0));
        publish(&mut writer);
    }
}

pub(super) fn location_at(pc: usize) -> Option<NativeLocation> {
    READERS.fetch_add(1, Ordering::SeqCst);
    let snapshot = SNAPSHOT.load(Ordering::SeqCst);
    let location = if snapshot.is_null() {
        None
    } else {
        // Writers defer freeing any snapshot while this reader is admitted.
        let snapshot = unsafe { &*snapshot };
        snapshot.images.iter().find_map(|image| {
            let offset = pc.checked_sub(image.base)?;
            if offset >= image.len {
                return None;
            }
            let (function, entry) = image.functions.iter().enumerate().find(|(_, entry)| {
                offset >= entry.offset as usize
                    && offset < entry.offset as usize + entry.len as usize
            })?;
            Some(NativeLocation {
                blob_hash: image.hash,
                function: u32::try_from(function).ok()?,
                code_offset: u32::try_from(offset - entry.offset as usize).ok()?,
            })
        })
    };
    READERS.fetch_sub(1, Ordering::SeqCst);
    location
}
