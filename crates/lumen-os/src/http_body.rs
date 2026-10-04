//! Incremental HTTP/1 response-body framing over an owned buffered transport.
//! Shared by language adapters; reads only occur when the caller requests a chunk.
use std::io::{self, BufRead, Read};

use lumen_common::http_body::Decoder;
pub use lumen_common::http_body::Framing;

pub fn read_capped_line(reader: &mut impl BufRead, budget: &mut usize) -> io::Result<String> {
    let mut bytes = Vec::new();
    let count = reader
        .by_ref()
        .take(*budget as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    if count > *budget {
        return Err(io::Error::other("HTTP line budget exceeded"));
    }
    *budget -= count;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn framing(method: &str, status: u16, headers: &[(String, String)]) -> io::Result<Framing> {
    lumen_common::http_body::framing(method, status, headers).map_err(io::Error::other)
}

/// Buffered blocking transport adapter over the shared framing state machine.
pub struct BodyReader<R> {
    reader: R,
    decoder: Decoder,
}
impl<R: BufRead> BodyReader<R> {
    pub fn new(reader: R, framing: Framing, limit: u64) -> io::Result<Self> {
        Ok(Self {
            reader,
            decoder: Decoder::new(framing, limit).map_err(io::Error::other)?,
        })
    }
    pub fn is_empty(&self) -> bool {
        self.decoder.is_done()
    }
    pub fn has_body(&self) -> bool {
        self.decoder.has_body()
    }
    pub fn read_chunk(&mut self, maximum: usize) -> io::Result<Option<Vec<u8>>> {
        if maximum == 0 {
            return Err(io::Error::other("HTTP chunk size must be positive"));
        }
        if self.decoder.is_done() {
            return Ok(None);
        }
        loop {
            let input = self.reader.fill_buf()?;
            let step = self
                .decoder
                .decode(input, input.is_empty(), maximum)
                .map_err(|error| {
                    if error.0.starts_with("truncated") {
                        io::Error::new(io::ErrorKind::UnexpectedEof, error)
                    } else {
                        io::Error::other(error)
                    }
                })?;
            self.reader.consume(step.consumed);
            if step.chunk.is_some() || step.done {
                return Ok(step.chunk);
            }
        }
    }
}
