//! Owned straight-alpha RGBA8 pixels. No renderer or operating-system dependency.
use alloc::vec::Vec;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rgba8Image {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Rgba8Image {
    pub fn is_valid(&self) -> bool {
        self.width != 0 && self.height != 0
            && (self.width as usize).checked_mul(self.height as usize)
                .and_then(|size| size.checked_mul(4)) == Some(self.pixels.len())
    }
}
