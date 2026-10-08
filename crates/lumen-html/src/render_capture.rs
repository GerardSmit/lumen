//! Bounded raster capture carriers shared by rendering clients.
use alloc::sync::Arc;
use crate::paint::ImageData;
use lumen_common::limits::{ByteBudget, ByteLease};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderCaptureTarget { Viewport }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderCaptureRequest {
    pub target: RenderCaptureTarget,
    pub width: u32,
    pub height: u32,
}

impl RenderCaptureRequest {
    /// Reserve the actual pixel carrier before invoking a raster backend.
    pub fn reserve(&self, budget: &Arc<ByteBudget>) -> Option<ByteLease> {
        if self.width == 0 || self.height == 0 { return None; }
        let bytes = (self.width as usize).checked_mul(self.height as usize)?.checked_mul(4)?;
        budget.reserve(bytes)
    }
}

/// The lease is owned by the same Arc as the pixels. Paint commands can outlive
/// the transition that produced them without releasing the reservation early.
pub struct ReservedImageData {
    pub image: ImageData,
    lease: ByteLease,
}

impl ReservedImageData {
    pub fn new(image: ImageData, lease: ByteLease) -> Option<Arc<Self>> {
        if !image.is_valid() || image.pixels.capacity() > lease.bytes() { return None; }
        Some(Arc::new(Self { image, lease }))
    }
    pub fn reserved_bytes(&self) -> usize { self.lease.bytes() }
}
impl core::fmt::Debug for ReservedImageData {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("ReservedImageData").field("image", &self.image)
            .field("reserved_bytes", &self.lease.bytes()).finish()
    }
}
impl PartialEq for ReservedImageData {
    fn eq(&self, other: &Self) -> bool { self.image == other.image }
}
