use super::*;

fn sample(len: usize) -> Vec<u8> {
    let mut state = 0x9E37_79B9u32;
    let words: [&[u8]; 6] = [b"lumen ", b"python ", b"\xE8\x00\x00\x00\x00", b"xz ", b"decoder ", b"\x01\x02\x03\x04"];
    let mut out = Vec::new();
    while out.len() < len {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        if state >> 28 < 12 {
            out.extend_from_slice(words[(state >> 8) as usize % words.len()]);
        } else {
            out.push((state >> 16) as u8);
        }
    }
    out.truncate(len);
    out
}

fn encode_all(mut z: XzStream, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 4096];
    for piece in data.chunks(1000) {
        let mut at = 0;
        while at < piece.len() {
            let s = z.code(false, &piece[at..], &mut buf);
            assert!(s.status.is_ok());
            at += s.consumed;
            out.extend_from_slice(&buf[..s.produced]);
        }
    }
    loop {
        let s = z.code(true, &[], &mut buf);
        out.extend_from_slice(&buf[..s.produced]);
        if s.status == Ok(XzStatus::StreamEnd) {
            return out;
        }
        assert!(s.status.is_ok());
    }
}

/// Feeds `input` in pieces of `step` bytes; returns the output and the unused tail.
fn decode_pieces(mut z: XzStream, input: &[u8], step: usize) -> Result<(Vec<u8>, Vec<u8>, bool), XzError> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 777];
    let mut ended = false;
    let mut at = 0;
    while at < input.len() && !ended {
        let end = (at + step).min(input.len());
        let mut piece = &input[at..end];
        let mut taken = 0;
        loop {
            let s = z.code(false, piece, &mut buf);
            let status = s.status?;
            piece = &piece[s.consumed..];
            taken += s.consumed;
            out.extend_from_slice(&buf[..s.produced]);
            if status == XzStatus::StreamEnd {
                ended = true;
                break;
            }
            if piece.is_empty() && s.produced < buf.len() {
                break;
            }
        }
        at += taken;
        if ended {
            return Ok((out, input[at..].to_vec(), true));
        }
    }
    Ok((out, Vec::new(), ended))
}

fn lzma2(preset: u32) -> Filter {
    Filter { id: FILTER_LZMA2, options: FilterOptions::Lzma(LzmaOptions::preset(preset).unwrap()) }
}

#[test]
fn xz_round_trip_in_small_pieces() {
    let data = sample(50_000);
    for check in [CHECK_NONE, CHECK_CRC32, CHECK_CRC64, CHECK_SHA256] {
        let packed = encode_all(XzStream::easy_encoder(3, check).unwrap(), &data);
        for step in [1usize, 7, 4096, packed.len()] {
            let (out, rest, ended) = decode_pieces(XzStream::stream_decoder(u64::MAX, TELL_CHECKS).unwrap(), &packed, step).unwrap();
            assert!(ended, "check {check} step {step}");
            assert_eq!(out, data, "check {check} step {step}");
            assert!(rest.is_empty());
        }
    }
}

#[test]
fn xz_empty_and_trailing_data() {
    let packed = encode_all(XzStream::easy_encoder(6, CHECK_CRC64).unwrap(), b"");
    assert_eq!(packed.len(), 32);
    let mut with_tail = packed.clone();
    with_tail.extend_from_slice(b"tail!");
    let (out, rest, ended) = decode_pieces(XzStream::auto_decoder(u64::MAX, TELL_CHECKS).unwrap(), &with_tail, 5).unwrap();
    assert!(ended && out.is_empty());
    assert_eq!(rest, b"tail!");
}

#[test]
fn filter_chains_round_trip() {
    let data = sample(30_000);
    let chain = [
        Filter { id: FILTER_DELTA, options: FilterOptions::Delta { dist: 2 } },
        Filter { id: FILTER_X86, options: FilterOptions::Bcj { start_offset: None } },
        lzma2(1),
    ];
    let packed = encode_all(XzStream::stream_encoder(&chain, CHECK_CRC32).unwrap(), &data);
    let (out, _, ended) = decode_pieces(XzStream::stream_decoder(u64::MAX, 0).unwrap(), &packed, 333).unwrap();
    assert!(ended);
    assert_eq!(out, data);
    let raw = encode_all(XzStream::raw_encoder(&chain).unwrap(), &data);
    let (out, _, ended) = decode_pieces(XzStream::raw_decoder(&chain).unwrap(), &raw, 50).unwrap();
    assert!(ended);
    assert_eq!(out, data);
}

#[test]
fn alone_and_raw_lzma1_round_trip() {
    let data = sample(20_000);
    let options = LzmaOptions::preset(2).unwrap();
    let packed = encode_all(XzStream::alone_encoder(&options).unwrap(), &data);
    for step in [1usize, 13, 5000] {
        let (out, rest, ended) = decode_pieces(XzStream::alone_decoder(u64::MAX).unwrap(), &packed, step).unwrap();
        assert!(ended, "step {step}");
        assert_eq!(out, data);
        assert!(rest.is_empty());
    }
    let (out, _, ended) = decode_pieces(XzStream::auto_decoder(u64::MAX, 0).unwrap(), &packed, 100).unwrap();
    assert!(ended);
    assert_eq!(out, data);

    let chain = [Filter { id: FILTER_LZMA1, options: FilterOptions::Lzma(options) }];
    let raw = encode_all(XzStream::raw_encoder(&chain).unwrap(), &data);
    let (out, _, ended) = decode_pieces(XzStream::raw_decoder(&chain).unwrap(), &raw, 9).unwrap();
    assert!(ended);
    assert_eq!(out, data);
}

#[test]
fn reads_streams_written_by_other_encoders() {
    let data = sample(40_000);
    let mut options = lr::XzOptions::with_preset(4);
    options.set_check_sum_type(lr::CheckType::Crc64);
    options.prepend_pre_filter(lr::FilterType::BcjX86, 0);
    let mut w = lr::XzWriter::new(Vec::new(), options).unwrap();
    w.write_all(&data).unwrap();
    let packed = w.finish().unwrap();
    let (out, _, ended) = decode_pieces(XzStream::stream_decoder(u64::MAX, TELL_CHECKS).unwrap(), &packed, 211).unwrap();
    assert!(ended);
    assert_eq!(out, data);
}

#[test]
fn corrupt_input_is_reported() {
    let data = sample(5000);
    let packed = encode_all(XzStream::easy_encoder(6, CHECK_CRC32).unwrap(), &data);
    let mut bad = packed.clone();
    let mid = bad.len() / 2;
    bad[mid] ^= 0x55;
    assert!(decode_pieces(XzStream::stream_decoder(u64::MAX, 0).unwrap(), &bad, 100).is_err());
    assert_eq!(decode_pieces(XzStream::stream_decoder(u64::MAX, 0).unwrap(), b"not an xz stream", 100).unwrap_err(), XzError::Format);
    assert_eq!(decode_pieces(XzStream::auto_decoder(u64::MAX, 0).unwrap(), &[0x37u8; 40], 100).unwrap_err(), XzError::Format);
    let limited = decode_pieces(XzStream::stream_decoder(100, 0).unwrap(), &packed, 100);
    assert_eq!(limited.unwrap_err(), XzError::MemLimit);
}

#[test]
fn check_reports() {
    let packed = encode_all(XzStream::easy_encoder(0, CHECK_CRC64).unwrap(), b"hi");
    let mut z = XzStream::stream_decoder(u64::MAX, TELL_CHECKS).unwrap();
    let mut out = [0u8; 16];
    let s = z.code(false, &packed, &mut out);
    assert_eq!(s.status, Ok(XzStatus::GetCheck));
    assert_eq!(z.check(), CHECK_CRC64);
    let none = encode_all(XzStream::easy_encoder(0, CHECK_NONE).unwrap(), b"hi");
    let mut z = XzStream::stream_decoder(u64::MAX, TELL_CHECKS).unwrap();
    assert_eq!(z.code(false, &none, &mut out).status, Ok(XzStatus::NoCheck));
}

#[test]
fn properties_round_trip() {
    let f = lzma2(6);
    let props = encode_filter_properties(&f).unwrap();
    let back = decode_filter_properties(FILTER_LZMA2, &props).unwrap();
    assert!(matches!(back.options, FilterOptions::Lzma(b) if b.dict_size == 8 << 20));
    assert_eq!(decode_filter_properties(FILTER_DELTA, &[3]).unwrap().options, FilterOptions::Delta { dist: 4 });
    assert_eq!(encode_filter_properties(&Filter { id: FILTER_ARM, options: FilterOptions::Bcj { start_offset: Some(4) } }).unwrap(), vec![4, 0, 0, 0]);
    assert!(decode_filter_properties(FILTER_LZMA2, &[41]).is_err());
}

#[test]
fn presets_match_liblzma() {
    let p = LzmaOptions::preset(6).unwrap();
    assert_eq!((p.dict_size, p.lc, p.lp, p.pb, p.mode, p.nice_len, p.mf, p.depth), (8 << 20, 3, 0, 2, MODE_NORMAL, 64, MF_BT4, 0));
    let e = LzmaOptions::preset(9 | PRESET_EXTREME).unwrap();
    assert_eq!((e.nice_len, e.depth), (273, 512));
    assert!(LzmaOptions::preset(10).is_none());
}
