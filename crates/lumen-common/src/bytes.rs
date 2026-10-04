//! Shared ownership of a byte range, with zero-copy static and owned backing.
use std::ops::{Deref, Range};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum Bytes {
    Static(&'static [u8]),
    Owned {
        data: Arc<[u8]>,
        range: Range<usize>,
    },
}

impl Bytes {
    pub fn owned(data: Arc<[u8]>) -> Self {
        let len = data.len();
        Self::Owned {
            data,
            range: 0..len,
        }
    }

    pub fn slice(&self, range: Range<usize>) -> Option<Self> {
        self.get(range.clone())?;
        Some(match self {
            Self::Static(data) => Self::Static(&data[range]),
            Self::Owned { data, range: outer } => Self::Owned {
                data: data.clone(),
                range: outer.start + range.start..outer.start + range.end,
            },
        })
    }

    pub fn as_static(&self) -> Option<&'static [u8]> {
        match self {
            Self::Static(data) => Some(data),
            Self::Owned { .. } => None,
        }
    }
}

impl Deref for Bytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Self::Static(data) => data,
            Self::Owned { data, range } => &data[range.clone()],
        }
    }
}

impl From<&'static [u8]> for Bytes {
    fn from(data: &'static [u8]) -> Self {
        Self::Static(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_slices_share_and_release_owned_backing() {
        let data: Arc<[u8]> = Arc::from(&b"abcdef"[..]);
        let weak = Arc::downgrade(&data);
        let all = Bytes::owned(data);
        let part = all.slice(1..5).unwrap().slice(1..3).unwrap();
        assert_eq!(&*part, b"cd");
        assert!(part.slice(0..3).is_none());
        drop(all);
        assert!(weak.upgrade().is_some());
        drop(part);
        assert!(weak.upgrade().is_none());
        let part = Bytes::Static(b"abcdef").slice(1..3).unwrap();
        assert_eq!(part.as_static(), Some(&b"bc"[..]));
    }
}
