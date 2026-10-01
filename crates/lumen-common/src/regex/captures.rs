//! Capture spans of a successful match.

const INLINE_CAPTURES: usize = 4;

/// Capture spans as `(start, end)` element indices, group 0 first; `None` for an unset group.
pub enum Captures {
    Inline {
        len: u8,
        spans: [Option<(usize, usize)>; INLINE_CAPTURES],
    },
    Heap(Box<[Option<(usize, usize)>]>),
}

impl Captures {
    pub(super) fn from_slots(slots: &[Option<usize>], groups: usize) -> Self {
        let len = groups + 1;
        if len <= INLINE_CAPTURES {
            let mut inline = [None; INLINE_CAPTURES];
            for (group, span) in inline[..len].iter_mut().enumerate() {
                *span = match (slots[2 * group], slots[2 * group + 1]) {
                    (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
                    _ => None,
                };
            }
            Captures::Inline {
                len: len as u8,
                spans: inline,
            }
        } else {
            let mut spans = Vec::with_capacity(len);
            for group in 0..len {
                spans.push(match (slots[2 * group], slots[2 * group + 1]) {
                    (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
                    _ => None,
                });
            }
            Captures::Heap(spans.into_boxed_slice())
        }
    }

    pub(super) fn one(span: (usize, usize)) -> Self {
        let mut spans = [None; INLINE_CAPTURES];
        spans[0] = Some(span);
        Captures::Inline { len: 1, spans }
    }
}

impl std::ops::Deref for Captures {
    type Target = [Option<(usize, usize)>];
    fn deref(&self) -> &Self::Target {
        match self {
            Captures::Inline { len, spans } => &spans[..*len as usize],
            Captures::Heap(spans) => spans,
        }
    }
}

impl AsRef<[Option<(usize, usize)>]> for Captures {
    fn as_ref(&self) -> &[Option<(usize, usize)>] {
        self
    }
}
