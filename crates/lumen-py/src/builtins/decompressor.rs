//! The state machine behind the incremental decompressor objects of `_bz2`, `_lzma` and
//! `zlib._ZlibDecompressor` (`BZ2Decompressor`, `LZMADecompressor`, `_ZlibDecompressor`): pending
//! input, `eof`, `unused_data` and `needs_input`. A [`Codec`] runs one decompression pass for its
//! format; the rest is shared.

/// The first output window of a pass.
pub const INITIAL_BLOCK: usize = 8 * 1024;

/// Grows `out` when its window is exhausted; `false` once `limit` forbids more room.
pub fn make_room(out: &mut Vec<u8>, produced: usize, limit: Option<usize>) -> bool {
    if produced < out.len() {
        return true;
    }
    let grow = out.len().max(INITIAL_BLOCK);
    let len = match limit {
        Some(m) if produced >= m => return false,
        Some(m) => (out.len() + grow).min(m),
        None => out.len() + grow,
    };
    out.resize(len, 0);
    true
}

/// What one decompression pass over the pending input produced.
pub struct Chunk {
    pub out: Vec<u8>,
    /// Pending input the pass took.
    pub consumed: usize,
    /// The pass reached the end of the compressed stream.
    pub eof: bool,
    /// The output window was left with room (the codec ran out of input, not of output space).
    pub room: bool,
}

pub trait Codec {
    type Error;

    /// Whether a failed pass also leaves the object not asking for input.
    const ERROR_STOPS_INPUT: bool = false;

    /// Decompresses `input` into at most `max` bytes (unbounded when `None`).
    fn run(&mut self, input: &[u8], max: Option<usize>) -> Result<Chunk, Self::Error>;
}

pub enum Failure<E> {
    /// `decompress()` after the end of the stream.
    AtEof,
    Codec(E),
}

pub struct Decompressor<C: Codec> {
    pub codec: C,
    /// Input not yet consumed by the codec.
    pending: Vec<u8>,
    pub unused_data: Vec<u8>,
    pub eof: bool,
    pub needs_input: bool,
}

impl<C: Codec> Decompressor<C> {
    pub fn new(codec: C) -> Self {
        Decompressor { codec, pending: Vec::new(), unused_data: Vec::new(), eof: false, needs_input: true }
    }

    /// `decompress(data, max_length)`.
    pub fn decompress(&mut self, data: &[u8], max: Option<usize>) -> Result<Vec<u8>, Failure<C::Error>> {
        if self.eof {
            return Err(Failure::AtEof);
        }
        self.pending.extend_from_slice(data);
        match self.codec.run(&self.pending, max) {
            Err(e) => {
                self.pending.clear();
                if C::ERROR_STOPS_INPUT {
                    self.needs_input = false;
                }
                Err(Failure::Codec(e))
            }
            Ok(chunk) => {
                self.pending.drain(..chunk.consumed);
                if chunk.eof {
                    self.eof = true;
                    self.needs_input = false;
                    if !self.pending.is_empty() {
                        self.unused_data = std::mem::take(&mut self.pending);
                    }
                } else {
                    self.needs_input = self.pending.is_empty() && chunk.room;
                }
                Ok(chunk.out)
            }
        }
    }
}
