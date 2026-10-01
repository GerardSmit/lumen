//! Capture spans of a successful match.

const INLINE_CAPTURES: usize = 4;

/// Capture spans as `(start, end)` element indices, group 0 first; `None` for an unset group.
pub struct Captures {
    spans: Spans,
    last: Option<usize>,
}

enum Spans {
    Inline {
        len: u8,
        spans: [Option<(usize, usize)>; INLINE_CAPTURES],
    },
    Heap(Box<[Option<(usize, usize)>]>),
}

impl Captures {
    /// The capture group that closed last along the successful path (Python's `lastindex`),
    /// recorded by the Python dialect only.
    pub fn last_group(&self) -> Option<usize> {
        self.last
    }

    pub(super) fn from_slots(slots: &[Option<usize>], groups: usize) -> Self {
        let last = slots.get(2 * (groups + 1)).copied().flatten();
        Captures {
            spans: Spans::from_slots(slots, groups),
            last,
        }
    }

    pub(super) fn one(span: (usize, usize)) -> Self {
        let mut spans = [None; INLINE_CAPTURES];
        spans[0] = Some(span);
        Captures {
            spans: Spans::Inline { len: 1, spans },
            last: None,
        }
    }
}

impl Spans {
    fn from_slots(slots: &[Option<usize>], groups: usize) -> Self {
        let len = groups + 1;
        if len <= INLINE_CAPTURES {
            let mut inline = [None; INLINE_CAPTURES];
            for (group, span) in inline[..len].iter_mut().enumerate() {
                *span = match (slots[2 * group], slots[2 * group + 1]) {
                    (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
                    _ => None,
                };
            }
            Spans::Inline {
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
            Spans::Heap(spans.into_boxed_slice())
        }
    }
}

impl std::ops::Deref for Captures {
    type Target = [Option<(usize, usize)>];
    fn deref(&self) -> &Self::Target {
        match &self.spans {
            Spans::Inline { len, spans } => &spans[..*len as usize],
            Spans::Heap(spans) => spans,
        }
    }
}

impl AsRef<[Option<(usize, usize)>]> for Captures {
    fn as_ref(&self) -> &[Option<(usize, usize)>] {
        self
    }
}
