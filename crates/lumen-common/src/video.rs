//! Bounded MP4/AVC decode path for HTML video resources.
//!
//! The container parser is Sans-I/O and no-std; H.264 decoding is portable safe
//! Rust. This first backend deliberately accepts only MP4 `avc1` tracks and
//! returns actual RGBA frames. Other MP4 codecs and WebM are unsupported.

extern crate alloc;

use alloc::{format, string::String, sync::Arc, vec::Vec};
use rusty_h264_common::{
    YuvFrame,
    nal::{NalUnitType, emulation_unprevent},
};
use rusty_h264_decoder::{Decoder, Sps};
use shiguredo_mp4::{
    TrackKind,
    boxes::SampleEntry,
    demux::{Input, Mp4FileDemuxer},
};

pub const MAX_MP4_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_VIDEO_FRAMES: usize = 100_000;
pub const MAX_ANNEX_B_BYTES: usize = 32 * 1024 * 1024;
/// Upper bound for retained decoded RGBA frames in one resource.
pub const MAX_DECODED_VIDEO_BYTES: usize = 256 * 1024 * 1024;
const MAX_DIMENSION: usize = 4096;
const MAX_PIXELS: usize = 8 * 1024 * 1024;
const MAX_AVCC_SAMPLE_BYTES: usize = 16 * 1024 * 1024;
const MAX_REFERENCE_FRAMES: u32 = 16;
const MAX_DECODER_DPB_BYTES: usize = 96 * 1024 * 1024;
const MAX_ENCODED_AUDIO_BYTES: usize = 32 * 1024 * 1024;

struct SampleRecord {
    offset: usize,
    size: usize,
    length_size: usize,
    parameter_sets: Vec<Vec<u8>>,
    presentation_time_micros: u64,
    duration_micros: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoFrame {
    pub presentation_time_micros: u64,
    pub width: u32,
    pub height: u32,
    /// Tightly packed, straight-alpha RGBA8 pixels.
    pub rgba: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedMp4 {
    pub width: u32,
    pub height: u32,
    pub duration_micros: u64,
    pub frames: Vec<VideoFrame>,
    pub audio: Option<EncodedAacTrack>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedAacTrack {
    pub decoder_config: Vec<u8>,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub samples: Vec<EncodedAacSample>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedAacSample {
    pub presentation_time_micros: u64,
    pub data: Vec<u8>,
}

/// Decode one bounded MP4 AAC-LC track to interleaved signed PCM. The optional
/// `aac` feature keeps the standard-library decoder separate from the
/// no-std-capable MP4/H.264 demux and video decoder.
#[cfg(feature = "aac")]
pub fn decode_aac_lc(track: &EncodedAacTrack) -> Result<crate::audio::PcmAudio, String> {
    if track.samples.is_empty() || !(1..=2).contains(&track.channels) {
        return Err("AAC track is empty or has unsupported channels".into());
    }
    let maximum_samples = track
        .samples
        .len()
        .checked_mul(1024)
        .and_then(|frames| frames.checked_mul(usize::from(track.channels)))
        .filter(|samples| *samples <= crate::audio::MAX_PCM_SAMPLES)
        .ok_or_else(|| "AAC decoded samples exceed the PCM memory limit".to_owned())?;
    let mut decoder = rusty_aac::AacDecoder::with_config_bytes(&track.decoder_config)
        .map_err(|error| format!("invalid AAC decoder configuration: {error}"))?;
    if decoder.sbr_support() != rusty_aac::sbr::SbrSupport::NotPresent {
        return Err("HE-AAC SBR audio is not supported".into());
    }
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(maximum_samples)
        .map_err(|_| "could not allocate bounded AAC PCM output".to_owned())?;
    let mut sample_rate = None;
    for packet in &track.samples {
        let decoded = decoder
            .decode(&packet.data, None)
            .map_err(|error| format!("AAC packet decode failed: {error}"))?;
        if decoded.channels != track.channels
            || decoded.sample_rate != track.sample_rate_hz
            || decoded.samples.len() % usize::from(track.channels) != 0
        {
            return Err("AAC output format disagrees with the MP4 track".into());
        }
        if sample_rate.is_some_and(|rate| rate != decoded.sample_rate) {
            return Err("AAC sample rate changes are unsupported".into());
        }
        sample_rate = Some(decoded.sample_rate);
        if samples.len().saturating_add(decoded.samples.len()) > maximum_samples {
            return Err("AAC decoded output exceeds the PCM memory limit".into());
        }
        samples.extend(decoded.samples.iter().map(|sample| {
            let sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            (sample * i16::MAX as f32).round() as i16
        }));
    }
    let sample_rate = sample_rate.ok_or_else(|| "AAC track produced no decoded PCM".to_owned())?;
    Ok(crate::audio::PcmAudio {
        sample_rate,
        channels: track.channels,
        samples,
    })
}

struct EncodedAacRecord {
    offset: usize,
    size: usize,
    presentation_time_micros: u64,
}

/// Demux and decode one bounded MP4 resource containing an AVC (`avc1`) video
/// track and, when present, bounded AAC-LC packet/config data for the shared
/// audio decoder. MP4 sample timestamps are reordered into presentation order
/// after the decoder performs H.264 POC reordering.
pub fn decode_mp4_avc(bytes: &[u8]) -> Result<DecodedMp4, String> {
    if bytes.is_empty() || bytes.len() > MAX_MP4_BYTES {
        return Err("MP4 resource is empty or exceeds the video resource limit".into());
    }

    let mut demuxer = Mp4FileDemuxer::new();
    demuxer.handle_input(Input {
        position: 0,
        data: bytes,
    });
    let tracks = demuxer
        .tracks()
        .map_err(|error| format!("invalid MP4 track metadata: {error:?}"))?;
    let audio_tracks = tracks
        .iter()
        .filter(|track| track.kind == TrackKind::Audio)
        .map(|track| track.track_id)
        .collect::<Vec<_>>();
    if audio_tracks.len() > 1 {
        return Err("MP4 with multiple audio tracks is unsupported".into());
    }
    let audio_track = audio_tracks.first().copied();
    let video_track = tracks
        .iter()
        .find(|track| track.kind == TrackKind::Video)
        .map(|track| track.track_id)
        .ok_or_else(|| "MP4 resource has no video track".to_owned())?;

    let mut records = Vec::new();
    let mut dimensions = None;
    let mut length_size = None;
    let mut parameter_sets = Vec::new();
    let mut audio_records = Vec::new();
    let mut audio_config = None;
    let mut audio_sample_rate = 0u32;
    let mut audio_channels = 0u16;
    let mut encoded_audio_bytes = 0usize;
    let mut audio_duration_micros = 0u64;

    loop {
        let sample = demuxer
            .next_sample()
            .map_err(|error| format!("invalid MP4 sample table: {error:?}"))?;
        let Some(sample) = sample else { break };
        if Some(sample.track.track_id) == audio_track {
            if let Some(entry) = sample.sample_entry.as_ref() {
                let SampleEntry::Mp4a(mp4a) = entry else {
                    return Err("MP4 audio codec is unsupported (expected AAC-LC)".into());
                };
                let decoder_config = mp4a
                    .esds_box
                    .es
                    .dec_config_descr
                    .dec_specific_info
                    .as_ref()
                    .filter(|_| mp4a.esds_box.es.dec_config_descr.object_type_indication == 0x40)
                    .map(|config| config.payload.clone())
                    .ok_or_else(|| "MP4 AAC decoder configuration is missing".to_owned())?;
                if decoder_config.is_empty() || decoder_config.len() > 64 {
                    return Err("MP4 AAC decoder configuration is invalid".into());
                }
                let channels = mp4a.audio.channelcount;
                let sample_rate = u32::from(mp4a.audio.samplerate.integer);
                if !(1..=2).contains(&channels) || !(8_000..=192_000).contains(&sample_rate) {
                    return Err("MP4 AAC channel count or sample rate is unsupported".into());
                }
                if audio_config
                    .as_ref()
                    .is_some_and(|existing| existing != &decoder_config)
                {
                    return Err("MP4 AAC configuration changes are unsupported".into());
                }
                audio_config = Some(decoder_config);
                audio_sample_rate = sample_rate;
                audio_channels = channels;
            }
            if audio_config.is_none() {
                return Err("MP4 AAC configuration is missing".into());
            }
            if sample.data_size > MAX_AVCC_SAMPLE_BYTES {
                return Err("MP4 AAC sample exceeds the per-frame input limit".into());
            }
            encoded_audio_bytes = encoded_audio_bytes
                .checked_add(sample.data_size)
                .filter(|bytes| *bytes <= MAX_ENCODED_AUDIO_BYTES)
                .ok_or_else(|| "MP4 AAC packets exceed the audio input budget".to_owned())?;
            let start = usize::try_from(sample.data_offset)
                .map_err(|_| "MP4 audio sample offset is too large".to_owned())?;
            let end = start
                .checked_add(sample.data_size)
                .filter(|end| *end <= bytes.len())
                .ok_or_else(|| "MP4 audio sample points outside the resource".to_owned())?;
            if audio_records.len() >= MAX_VIDEO_FRAMES {
                return Err("MP4 AAC track has too many samples".into());
            }
            let timescale = u64::from(sample.track.timescale.get());
            let timestamp = u64::try_from(sample.timestamp)
                .map_err(|_| "MP4 AAC timestamp is invalid".to_owned())?;
            let presentation_time_micros = timestamp
                .checked_mul(1_000_000)
                .ok_or_else(|| "MP4 AAC timestamp overflow".to_owned())?
                / timescale;
            let audio_end = timestamp
                .checked_add(u64::from(sample.duration))
                .and_then(|end| end.checked_mul(1_000_000))
                .ok_or_else(|| "MP4 AAC duration overflow".to_owned())?
                / timescale;
            audio_duration_micros = audio_duration_micros.max(audio_end);
            audio_records.push(EncodedAacRecord {
                offset: start,
                size: end - start,
                presentation_time_micros,
            });
            continue;
        }
        if sample.track.track_id != video_track {
            continue;
        }
        if let Some(entry) = sample.sample_entry {
            let SampleEntry::Avc1(avc) = entry else {
                return Err("MP4 video codec is unsupported (expected avc1/H.264)".into());
            };
            let width = usize::from(avc.visual.width);
            let height = usize::from(avc.visual.height);
            if width == 0
                || height == 0
                || width > MAX_DIMENSION
                || height > MAX_DIMENSION
                || width
                    .checked_mul(height)
                    .is_none_or(|pixels| pixels > MAX_PIXELS)
            {
                return Err("MP4 video dimensions exceed the decoder limits".into());
            }
            let new_dimensions = (width as u32, height as u32);
            if dimensions.is_some_and(|previous| previous != new_dimensions) {
                return Err("MP4 AVC resolution changes are unsupported".into());
            }
            dimensions = Some(new_dimensions);
            length_size = Some(usize::from(avc.avcc_box.length_size_minus_one.get()) + 1);
            parameter_sets = avc
                .avcc_box
                .sps_list
                .iter()
                .chain(avc.avcc_box.pps_list.iter())
                .cloned()
                .collect();
            if parameter_sets.is_empty() {
                return Err("MP4 AVC config has no SPS/PPS parameter sets".into());
            }
            let mut reference_frames = 0u32;
            for sps_nal in avc.avcc_box.sps_list.iter() {
                if sps_nal
                    .first()
                    .is_none_or(|header| NalUnitType::from_id(*header) != NalUnitType::Sps)
                {
                    return Err("MP4 AVC config contains a malformed SPS NAL".into());
                }
                let rbsp = emulation_unprevent(&sps_nal[1..]);
                let sps = Sps::parse(&rbsp)
                    .map_err(|error| format!("invalid H.264 sequence parameters: {error:?}"))?;
                if sps.display_width() != width || sps.display_height() != height {
                    return Err("H.264 SPS dimensions do not match the MP4 sample entry".into());
                }
                reference_frames = reference_frames.max(sps.max_num_ref_frames);
            }
            if reference_frames > MAX_REFERENCE_FRAMES {
                return Err("H.264 reference-frame count exceeds the decoder limit".into());
            }
            let coded_pixels = avc
                .avcc_box
                .sps_list
                .first()
                .and_then(|sps_nal| {
                    let rbsp = emulation_unprevent(&sps_nal[1..]);
                    Sps::parse(&rbsp).ok()
                })
                .and_then(|sps| sps.coded_width().checked_mul(sps.coded_height()))
                .ok_or_else(|| "H.264 coded dimensions overflow".to_owned())?;
            let dpb_bytes = coded_pixels
                .checked_mul(3)
                .and_then(|bytes| bytes.checked_add(1))
                .map(|bytes| bytes / 2)
                .and_then(|frame_bytes| frame_bytes.checked_mul(reference_frames as usize + 2))
                .ok_or_else(|| "H.264 reference-frame memory estimate overflow".to_owned())?;
            if dpb_bytes > MAX_DECODER_DPB_BYTES {
                return Err("H.264 reference-frame memory exceeds the decoder budget".into());
            }
        }
        let length_size = length_size.ok_or_else(|| "MP4 AVC config is missing".to_owned())?;
        if sample.data_size > MAX_AVCC_SAMPLE_BYTES {
            return Err("MP4 AVC sample exceeds the per-frame decode limit".into());
        }
        let start = usize::try_from(sample.data_offset)
            .map_err(|_| "MP4 sample offset is too large".to_owned())?;
        start
            .checked_add(sample.data_size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "MP4 sample points outside the resource".to_owned())?;
        if records.len() >= MAX_VIDEO_FRAMES {
            return Err("MP4 video has too many frames".into());
        }
        let timescale = u64::from(sample.track.timescale.get());
        let pts =
            i128::from(sample.timestamp) + i128::from(sample.composition_time_offset.unwrap_or(0));
        let pts = u64::try_from(pts.max(0)).map_err(|_| "MP4 timestamp overflow".to_owned())?;
        let start_micros = pts
            .checked_mul(1_000_000)
            .ok_or_else(|| "MP4 timestamp overflow".to_owned())?
            / timescale;
        let duration_micros = u64::from(sample.duration)
            .checked_mul(1_000_000)
            .ok_or_else(|| "MP4 duration overflow".to_owned())?
            / timescale;
        records.push(SampleRecord {
            offset: start,
            size: sample.data_size,
            length_size,
            parameter_sets: if sample.sample_entry.is_some() {
                parameter_sets.clone()
            } else {
                Vec::new()
            },
            presentation_time_micros: start_micros,
            duration_micros,
        });
    }

    if records.is_empty() {
        return Err("MP4 AVC track contains no decodable video samples".into());
    }
    let (width, height) = dimensions.ok_or_else(|| "MP4 AVC dimensions are missing".to_owned())?;
    let audio = if audio_records.is_empty() {
        None
    } else {
        let decoder_config =
            audio_config.ok_or_else(|| "MP4 AAC decoder configuration is missing".to_owned())?;
        let mut samples = Vec::new();
        samples
            .try_reserve_exact(audio_records.len())
            .map_err(|_| "could not allocate bounded AAC sample index".to_owned())?;
        for record in audio_records {
            let mut data = Vec::new();
            data.try_reserve_exact(record.size)
                .map_err(|_| "could not allocate bounded AAC packet".to_owned())?;
            data.extend_from_slice(&bytes[record.offset..record.offset + record.size]);
            samples.push(EncodedAacSample {
                presentation_time_micros: record.presentation_time_micros,
                data,
            });
        }
        Some(EncodedAacTrack {
            decoder_config,
            sample_rate_hz: audio_sample_rate,
            channels: audio_channels,
            samples,
        })
    };
    let projected_rgba_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|frame_bytes| frame_bytes.checked_mul(records.len()))
        .ok_or_else(|| "decoded video memory estimate overflow".to_owned())?;
    if projected_rgba_bytes > MAX_DECODED_VIDEO_BYTES {
        return Err("MP4 video exceeds the decoded frame memory limit".into());
    }

    // Decode one access unit at a time. The codec's DPB is retained, while each
    // temporary YUV frame is converted and released before the next sample.
    let mut decoder = Decoder::new();
    let mut frames = Vec::new();
    frames
        .try_reserve_exact(records.len())
        .map_err(|_| "could not allocate bounded video frame index".to_owned())?;
    let mut duration_micros = 0;
    for record in &records {
        let mut access_unit = Vec::new();
        let parameter_bytes = record
            .parameter_sets
            .iter()
            .try_fold(0usize, |sum, nal| {
                sum.checked_add(4)?.checked_add(nal.len())
            })
            .ok_or_else(|| "AVC parameter-set size overflow".to_owned())?;
        let capacity = parameter_bytes
            .checked_add(record.size)
            .filter(|size| *size <= MAX_ANNEX_B_BYTES)
            .ok_or_else(|| "MP4 AVC access unit exceeds the decode input limit".to_owned())?;
        access_unit
            .try_reserve_exact(capacity)
            .map_err(|_| "could not allocate bounded H.264 access unit".to_owned())?;
        for nal in &record.parameter_sets {
            access_unit.extend_from_slice(&[0, 0, 0, 1]);
            access_unit.extend_from_slice(nal);
        }
        append_avcc_sample(
            &mut access_unit,
            &bytes[record.offset..record.offset + record.size],
            record.length_size,
        )?;
        if let Some(frame) = decoder
            .decode(&access_unit)
            .map_err(|error| format!("H.264 decode failed: {error:?}"))?
        {
            if frame.width != width as usize || frame.height != height as usize {
                return Err("H.264 decoded dimensions do not match the MP4 sample entry".into());
            }
            let rgba = yuv420_to_rgba(&frame)?;
            duration_micros = duration_micros.max(
                record
                    .presentation_time_micros
                    .saturating_add(record.duration_micros),
            );
            frames.push(VideoFrame {
                presentation_time_micros: record.presentation_time_micros,
                width,
                height,
                rgba: Arc::new(rgba),
            });
        }
    }
    duration_micros = duration_micros.max(audio_duration_micros);
    if frames.len() != records.len() {
        return Err(format!(
            "H.264 decoder returned {} frames for {} MP4 samples",
            frames.len(),
            records.len()
        ));
    }
    frames.sort_unstable_by_key(|frame| frame.presentation_time_micros);
    Ok(DecodedMp4 {
        width,
        height,
        duration_micros,
        frames,
        audio,
    })
}

fn append_avcc_sample(
    output: &mut Vec<u8>,
    sample: &[u8],
    length_size: usize,
) -> Result<(), String> {
    if !(1..=4).contains(&length_size) {
        return Err("MP4 AVC NAL length size is invalid".into());
    }
    let mut cursor = 0usize;
    while cursor < sample.len() {
        let length_end = cursor
            .checked_add(length_size)
            .filter(|end| *end <= sample.len())
            .ok_or_else(|| "truncated MP4 AVC NAL length".to_owned())?;
        let mut nal_size = 0usize;
        for byte in &sample[cursor..length_end] {
            nal_size = nal_size
                .checked_mul(256)
                .and_then(|size| size.checked_add(usize::from(*byte)))
                .ok_or_else(|| "MP4 AVC NAL size overflow".to_owned())?;
        }
        cursor = length_end;
        let nal_end = cursor
            .checked_add(nal_size)
            .filter(|end| nal_size > 0 && *end <= sample.len())
            .ok_or_else(|| "truncated or empty MP4 AVC NAL unit".to_owned())?;
        if output
            .len()
            .checked_add(4)
            .and_then(|size| size.checked_add(nal_size))
            .is_none_or(|size| size > MAX_ANNEX_B_BYTES)
        {
            return Err("MP4 AVC elementary stream exceeds the decode input limit".into());
        }
        output.extend_from_slice(&[0, 0, 0, 1]);
        output.extend_from_slice(&sample[cursor..nal_end]);
        cursor = nal_end;
    }
    Ok(())
}

fn yuv420_to_rgba(frame: &YuvFrame) -> Result<Vec<u8>, String> {
    let width = frame.width;
    let height = frame.height;
    let pixel_count = width
        .checked_mul(height)
        .filter(|pixels| *pixels <= MAX_PIXELS)
        .ok_or_else(|| "decoded H.264 frame dimensions exceed the pixel limit".to_owned())?;
    let chroma_width = width.div_ceil(2);
    if frame.y.len() < pixel_count
        || frame.u.len() < chroma_width * height.div_ceil(2)
        || frame.v.len() < chroma_width * height.div_ceil(2)
    {
        return Err("H.264 decoder returned incomplete YUV planes".into());
    }
    let rgba_len = pixel_count
        .checked_mul(4)
        .ok_or_else(|| "decoded RGBA frame size overflow".to_owned())?;
    let mut rgba = Vec::new();
    rgba.try_reserve_exact(rgba_len)
        .map_err(|_| "could not allocate bounded RGBA frame".to_owned())?;
    for y in 0..height {
        for x in 0..width {
            let luma = i32::from(frame.y[y * width + x]) - 16;
            let cb = i32::from(frame.u[(y / 2) * chroma_width + x / 2]) - 128;
            let cr = i32::from(frame.v[(y / 2) * chroma_width + x / 2]) - 128;
            let c = luma.max(0);
            rgba.push(clamp_channel((298 * c + 409 * cr + 128) >> 8));
            rgba.push(clamp_channel((298 * c - 100 * cb - 208 * cr + 128) >> 8));
            rgba.push(clamp_channel((298 * c + 516 * cb + 128) >> 8));
            rgba.push(255);
        }
    }
    Ok(rgba)
}

fn clamp_channel(channel: i32) -> u8 {
    channel.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn converts_length_prefixed_nals_to_annex_b() {
        let mut output = Vec::new();
        append_avcc_sample(&mut output, &[0, 0, 0, 2, 0x65, 0x88], 4).unwrap();
        assert_eq!(output, [0, 0, 0, 1, 0x65, 0x88]);
    }

    #[test]
    fn rejects_truncated_mp4_nal() {
        assert!(append_avcc_sample(&mut Vec::new(), &[0, 0, 0, 4, 0x65], 4).is_err());
    }

    #[test]
    fn converts_yuv420_pixels_to_rgba() {
        let frame = YuvFrame {
            width: 2,
            height: 2,
            y: vec![235; 4],
            u: vec![128],
            v: vec![128],
        };
        assert_eq!(
            yuv420_to_rgba(&frame).unwrap(),
            [255, 255, 255, 255].repeat(4)
        );
    }

    #[test]
    fn h264_backend_decodes_a_real_access_unit() {
        use rusty_h264::{Encoder, EncoderConfig};

        let frame = YuvFrame {
            width: 32,
            height: 32,
            y: vec![96; 32 * 32],
            u: vec![128; 16 * 16],
            v: vec![128; 16 * 16],
        };
        let mut encoder = Encoder::new(EncoderConfig::new(32, 32)).unwrap();
        let mut stream = encoder.encode(&frame);
        stream.extend_from_slice(&encoder.flush());
        let decoded = Decoder::new().decode_stream(&stream).unwrap();
        assert_eq!(decoded.len(), 1);
        assert_eq!((decoded[0].width, decoded[0].height), (32, 32));
        assert_eq!(decoded[0].y, frame.y);
        assert_eq!(decoded[0].u, frame.u);
        assert_eq!(decoded[0].v, frame.v);
    }

    #[test]
    fn decodes_real_avc_pixels_through_mp4_demuxer() {
        use core::num::NonZeroU32;
        use rusty_h264::{Encoder, EncoderConfig};
        use shiguredo_mp4::{
            TrackKind, Uint,
            boxes::{Avc1Box, AvccBox, SampleEntry, VisualSampleEntryFields},
            mux::{Mp4FileMuxer, Sample},
        };

        let source = YuvFrame {
            width: 32,
            height: 32,
            y: vec![96; 32 * 32],
            u: vec![128; 16 * 16],
            v: vec![128; 16 * 16],
        };
        let mut encoder = Encoder::new(EncoderConfig::new(32, 32)).unwrap();
        let mut annex_b = encoder.encode(&source);
        annex_b.extend_from_slice(&encoder.flush());
        let nals = split_annex_b_for_test(&annex_b);
        let sps = nals
            .iter()
            .find(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 7))
            .expect("encoder emits SPS")
            .to_vec();
        let pps = nals
            .iter()
            .find(|nal| nal.first().is_some_and(|byte| byte & 0x1f == 8))
            .expect("encoder emits PPS")
            .to_vec();
        let mut payload = Vec::new();
        for nal in nals.iter().filter(|nal| {
            nal.first()
                .is_some_and(|byte| byte & 0x1f != 7 && byte & 0x1f != 8)
        }) {
            payload.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            payload.extend_from_slice(nal);
        }
        let sample_entry = SampleEntry::Avc1(Avc1Box {
            visual: VisualSampleEntryFields {
                data_reference_index: VisualSampleEntryFields::DEFAULT_DATA_REFERENCE_INDEX,
                width: 32,
                height: 32,
                horizresolution: VisualSampleEntryFields::DEFAULT_HORIZRESOLUTION,
                vertresolution: VisualSampleEntryFields::DEFAULT_VERTRESOLUTION,
                frame_count: VisualSampleEntryFields::DEFAULT_FRAME_COUNT,
                compressorname: VisualSampleEntryFields::NULL_COMPRESSORNAME,
                depth: VisualSampleEntryFields::DEFAULT_DEPTH,
            },
            avcc_box: AvccBox {
                avc_profile_indication: sps[1],
                profile_compatibility: sps[2],
                avc_level_indication: sps[3],
                length_size_minus_one: Uint::new(3),
                sps_list: vec![sps],
                pps_list: vec![pps],
                chroma_format: Some(Uint::new(1)),
                bit_depth_luma_minus8: Some(Uint::new(0)),
                bit_depth_chroma_minus8: Some(Uint::new(0)),
                sps_ext_list: Vec::new(),
            },
            unknown_boxes: Vec::new(),
        });
        let timescale = NonZeroU32::new(30).unwrap();
        let mut muxer = Mp4FileMuxer::new().unwrap();
        let mut mp4 = muxer.initial_boxes_bytes().to_vec();
        let sample = Sample {
            track_kind: TrackKind::Video,
            sample_entry: Some(sample_entry),
            keyframe: true,
            timescale,
            duration: 1,
            composition_time_offset: None,
            data_offset: mp4.len() as u64,
            data_size: payload.len(),
        };
        muxer.append_sample(&sample).unwrap();
        mp4.extend_from_slice(&payload);
        let finalized = muxer.finalize().unwrap();
        for (offset, data) in finalized.offset_and_bytes_pairs() {
            let start = offset as usize;
            let end = start + data.len();
            if mp4.len() < end {
                mp4.resize(end, 0);
            }
            mp4[start..end].copy_from_slice(data);
        }

        let decoded = decode_mp4_avc(&mp4).unwrap();
        assert_eq!((decoded.width, decoded.height), (32, 32));
        assert_eq!(decoded.frames.len(), 1);
        assert_eq!(decoded.frames[0].presentation_time_micros, 0);
        assert_eq!(decoded.frames[0].rgba.len(), 32 * 32 * 4);
        assert!(decoded.frames[0].rgba[3] == 255);
    }

    #[cfg(feature = "aac")]
    #[test]
    fn aac_lc_decoder_rejects_a_malformed_access_unit() {
        let track = EncodedAacTrack {
            decoder_config: vec![0x12, 0x10], // AAC-LC, 44.1 kHz, stereo.
            sample_rate_hz: 44_100,
            channels: 2,
            samples: vec![EncodedAacSample {
                presentation_time_micros: 0,
                data: vec![0],
            }],
        };
        assert!(decode_aac_lc(&track).is_err());
    }

    fn split_annex_b_for_test(bytes: &[u8]) -> Vec<&[u8]> {
        let mut starts = Vec::new();
        let mut index = 0;
        while index + 3 < bytes.len() {
            let marker = if bytes[index..].starts_with(&[0, 0, 0, 1]) {
                Some(4)
            } else if bytes[index..].starts_with(&[0, 0, 1]) {
                Some(3)
            } else {
                None
            };
            if let Some(size) = marker {
                starts.push((index, size));
                index += size;
            } else {
                index += 1;
            }
        }
        starts
            .iter()
            .enumerate()
            .filter_map(|(position, (start, marker_size))| {
                let begin = start + marker_size;
                let mut end = starts
                    .get(position + 1)
                    .map_or(bytes.len(), |(next, _)| *next);
                while end > begin && bytes[end - 1] == 0 {
                    end -= 1;
                }
                (begin < end).then_some(&bytes[begin..end])
            })
            .collect()
    }
}
