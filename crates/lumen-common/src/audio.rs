//! Bounded, language-neutral PCM audio data and RIFF/WAVE decoding.
//!
//! This first media backend intentionally accepts uncompressed integer PCM
//! WAV only. It does not depend on a host, operating system, or audio device.

extern crate alloc;

use alloc::vec::Vec;

pub const MAX_WAV_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_PCM_SAMPLES: usize = 10 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PcmAudio {
    pub sample_rate: u32,
    pub channels: u16,
    /// Interleaved signed samples. Each item is one channel sample.
    pub samples: Vec<i16>,
}

impl PcmAudio {
    pub fn frame_count(&self) -> usize {
        self.samples.len() / usize::from(self.channels)
    }

    pub fn duration_seconds(&self) -> f64 {
        self.frame_count() as f64 / f64::from(self.sample_rate)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WavError {
    TooLarge,
    InvalidContainer,
    MissingFormat,
    MissingData,
    UnsupportedEncoding,
    InvalidFormat,
    InvalidSamples,
}

/// Decode bounded RIFF/WAVE PCM with one or two channels and 8- or 16-bit
/// integer samples. Unknown chunks are skipped according to RIFF padding.
pub fn decode_wav(bytes: &[u8]) -> Result<PcmAudio, WavError> {
    if bytes.len() > MAX_WAV_BYTES {
        return Err(WavError::TooLarge);
    }
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::InvalidContainer);
    }
    let declared = read_u32(bytes, 4).ok_or(WavError::InvalidContainer)? as usize;
    let end = declared
        .checked_add(8)
        .filter(|end| *end <= bytes.len())
        .ok_or(WavError::InvalidContainer)?;
    let mut offset = 12usize;
    let mut format = None;
    let mut data = None;
    while offset
        .checked_add(8)
        .is_some_and(|header_end| header_end <= end)
    {
        let id = &bytes[offset..offset + 4];
        let size = read_u32(bytes, offset + 4).ok_or(WavError::InvalidContainer)? as usize;
        let content_start = offset + 8;
        let content_end = content_start
            .checked_add(size)
            .filter(|stop| *stop <= end)
            .ok_or(WavError::InvalidContainer)?;
        let content = &bytes[content_start..content_end];
        if id == b"fmt " {
            if content.len() < 16 {
                return Err(WavError::InvalidFormat);
            }
            format = Some((
                read_u16(content, 0).ok_or(WavError::InvalidFormat)?,
                read_u16(content, 2).ok_or(WavError::InvalidFormat)?,
                read_u32(content, 4).ok_or(WavError::InvalidFormat)?,
                read_u32(content, 8).ok_or(WavError::InvalidFormat)?,
                read_u16(content, 12).ok_or(WavError::InvalidFormat)?,
                read_u16(content, 14).ok_or(WavError::InvalidFormat)?,
            ));
        } else if id == b"data" && data.is_none() {
            data = Some(content);
        }
        let padded_end = content_end
            .checked_add(size & 1)
            .filter(|stop| *stop <= end)
            .ok_or(WavError::InvalidContainer)?;
        offset = padded_end;
    }
    if offset != end {
        return Err(WavError::InvalidContainer);
    }
    let (encoding, channels, sample_rate, byte_rate, block_align, bits) =
        format.ok_or(WavError::MissingFormat)?;
    let data = data.ok_or(WavError::MissingData)?;
    if encoding != 1 {
        return Err(WavError::UnsupportedEncoding);
    }
    if !(channels == 1 || channels == 2)
        || !(8_000..=192_000).contains(&sample_rate)
        || !(bits == 8 || bits == 16)
    {
        return Err(WavError::InvalidFormat);
    }
    let bytes_per_sample = usize::from(bits / 8);
    let expected_align = usize::from(channels) * bytes_per_sample;
    let expected_rate = sample_rate.checked_mul(expected_align as u32);
    if usize::from(block_align) != expected_align
        || expected_rate != Some(byte_rate)
        || data.len() % expected_align != 0
    {
        return Err(WavError::InvalidSamples);
    }
    let sample_count = data.len() / bytes_per_sample;
    if sample_count > MAX_PCM_SAMPLES {
        return Err(WavError::TooLarge);
    }
    let mut samples = Vec::with_capacity(sample_count);
    if bits == 8 {
        samples.extend(data.iter().map(|sample| (i16::from(*sample) - 128) << 8));
    } else {
        samples.extend(
            data.chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]])),
        );
    }
    Ok(PcmAudio {
        sample_rate,
        channels,
        samples,
    })
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn wav(channels: u16, bits: u16, data: &[u8]) -> Vec<u8> {
        let align = channels * (bits / 8);
        let mut bytes = vec![];
        bytes.extend_from_slice(b"RIFF");
        let riff_size = 36 + data.len() as u32 + (data.len() as u32 & 1);
        bytes.extend_from_slice(&riff_size.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&44_100u32.to_le_bytes());
        bytes.extend_from_slice(&(44_100 * u32::from(align)).to_le_bytes());
        bytes.extend_from_slice(&align.to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        if data.len() & 1 != 0 {
            bytes.push(0);
        }
        bytes
    }

    #[test]
    fn decodes_mono_unsigned_eight_bit_samples() {
        let audio = decode_wav(&wav(1, 8, &[0, 128, 255])).unwrap();
        assert_eq!(audio.sample_rate, 44_100);
        assert_eq!(audio.channels, 1);
        assert_eq!(audio.samples, [-32768, 0, 32512]);
        assert_eq!(audio.frame_count(), 3);
    }

    #[test]
    fn decodes_stereo_sixteen_bit_samples() {
        let audio = decode_wav(&wav(2, 16, &[1, 0, 254, 255])).unwrap();
        assert_eq!(audio.samples, [1, -2]);
        assert_eq!(audio.duration_seconds(), 1.0 / 44_100.0);
    }

    #[test]
    fn rejects_bad_alignment_and_compressed_formats() {
        assert_eq!(
            decode_wav(&wav(2, 16, &[1, 0, 0])).unwrap_err(),
            WavError::InvalidSamples
        );
        let mut compressed = wav(1, 8, &[128]);
        compressed[20..22].copy_from_slice(&3u16.to_le_bytes());
        assert_eq!(
            decode_wav(&compressed).unwrap_err(),
            WavError::UnsupportedEncoding
        );
        let mut missing_padding = wav(1, 8, &[128]);
        missing_padding.pop();
        assert_eq!(
            decode_wav(&missing_padding).unwrap_err(),
            WavError::InvalidContainer
        );
    }
}
