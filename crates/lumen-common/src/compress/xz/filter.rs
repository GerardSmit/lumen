//! The delta and branch/call/jump (BCJ) filters of `.xz`, in both directions.
//!
//! Every filter works on a byte stream that arrives in pieces. [`Filter::run`] takes the next
//! piece and returns the bytes that are final; a branch filter holds back the few trailing bytes
//! that a later piece may still change, and [`Filter::run`] with `last` releases them unchanged.

use super::{FILTER_ARM, FILTER_ARMTHUMB, FILTER_DELTA, FILTER_IA64, FILTER_POWERPC, FILTER_SPARC, FILTER_X86};

pub(super) struct Filter {
    kind: Kind,
    encode: bool,
    pos: u32,
    carry: Vec<u8>,
}

enum Kind {
    Delta { hist: Vec<u8>, at: usize },
    X86 { prev_mask: u32, prev_pos: u32 },
    Simple(fn(&mut [u8], u32, bool) -> usize),
}

impl Filter {
    pub(super) fn delta(distance: u32, encode: bool) -> Filter {
        let dist = distance.clamp(1, 256) as usize;
        Filter { kind: Kind::Delta { hist: vec![0; dist], at: 0 }, encode, pos: 0, carry: Vec::new() }
    }

    /// The BCJ filter with id `id`, or `None` when `id` is not one.
    pub(super) fn bcj(id: u64, start_offset: u32, encode: bool) -> Option<Filter> {
        let kind = match id {
            FILTER_X86 => Kind::X86 { prev_mask: 0, prev_pos: 0u32.wrapping_sub(5) },
            FILTER_POWERPC => Kind::Simple(powerpc),
            FILTER_IA64 => Kind::Simple(ia64),
            FILTER_ARM => Kind::Simple(arm),
            FILTER_ARMTHUMB => Kind::Simple(arm_thumb),
            FILTER_SPARC => Kind::Simple(sparc),
            _ => return None,
        };
        Some(Filter { kind, encode, pos: start_offset, carry: Vec::new() })
    }

    pub(super) fn is_filter_id(id: u64) -> bool {
        matches!(id, FILTER_DELTA | FILTER_X86 | FILTER_POWERPC | FILTER_IA64 | FILTER_ARM | FILTER_ARMTHUMB | FILTER_SPARC)
    }

    /// The bytes of `data` (appended to what was held back) that are final.
    pub(super) fn run(&mut self, mut data: Vec<u8>, last: bool) -> Vec<u8> {
        if let Kind::Delta { hist, at } = &mut self.kind {
            for b in data.iter_mut() {
                let prev = hist[*at];
                let cur = *b;
                if self.encode {
                    hist[*at] = cur;
                    *b = cur.wrapping_sub(prev);
                } else {
                    let out = cur.wrapping_add(prev);
                    hist[*at] = out;
                    *b = out;
                }
                *at += 1;
                if *at == hist.len() {
                    *at = 0;
                }
            }
            return data;
        }
        if !self.carry.is_empty() {
            let mut joined = std::mem::take(&mut self.carry);
            joined.extend_from_slice(&data);
            data = joined;
        }
        let done = match &mut self.kind {
            Kind::X86 { prev_mask, prev_pos } => x86(&mut data, self.pos, self.encode, prev_mask, prev_pos),
            Kind::Simple(code) => code(&mut data, self.pos, self.encode),
            Kind::Delta { .. } => unreachable!(),
        };
        self.pos = self.pos.wrapping_add(done as u32);
        if !last {
            self.carry = data.split_off(done);
        }
        data
    }
}

fn x86(buf: &mut [u8], now_pos: u32, encode: bool, prev_mask: &mut u32, prev_pos: &mut u32) -> usize {
    const ALLOWED: [bool; 8] = [true, true, true, false, true, false, false, false];
    const BIT_NUMBER: [u32; 8] = [0, 1, 2, 2, 3, 3, 3, 3];
    let ms_byte = |b: u8| b == 0 || b == 0xFF;
    if buf.len() < 5 {
        return 0;
    }
    if now_pos.wrapping_sub(*prev_pos) > 5 {
        *prev_pos = now_pos.wrapping_sub(5);
    }
    let limit = buf.len() - 5;
    let mut at = 0usize;
    while at <= limit {
        let b = buf[at];
        if b != 0xE8 && b != 0xE9 {
            at += 1;
            continue;
        }
        let offset = now_pos.wrapping_add(at as u32).wrapping_sub(*prev_pos);
        *prev_pos = now_pos.wrapping_add(at as u32);
        if offset > 5 {
            *prev_mask = 0;
        } else {
            for _ in 0..offset {
                *prev_mask &= 0x77;
                *prev_mask <<= 1;
            }
        }
        let b4 = buf[at + 4];
        if ms_byte(b4) && ALLOWED[((*prev_mask >> 1) & 7) as usize] && (*prev_mask >> 1) < 0x10 {
            let mut src = u32::from_le_bytes([buf[at + 1], buf[at + 2], buf[at + 3], b4]);
            let mut dest;
            loop {
                let here = now_pos.wrapping_add(at as u32).wrapping_add(5);
                dest = if encode { src.wrapping_add(here) } else { src.wrapping_sub(here) };
                if *prev_mask == 0 {
                    break;
                }
                let i = BIT_NUMBER[(*prev_mask >> 1) as usize];
                let b = (dest >> (24 - i * 8)) as u8;
                if !ms_byte(b) {
                    break;
                }
                src = dest ^ ((1u32 << (32 - i * 8)) - 1);
            }
            buf[at + 4] = !(((dest >> 24) & 1).wrapping_sub(1)) as u8;
            buf[at + 3] = (dest >> 16) as u8;
            buf[at + 2] = (dest >> 8) as u8;
            buf[at + 1] = dest as u8;
            at += 5;
            *prev_mask = 0;
        } else {
            at += 1;
            *prev_mask |= 1;
            if ms_byte(buf[at + 3]) {
                *prev_mask |= 0x10;
            }
        }
    }
    at
}

fn arm(buf: &mut [u8], now_pos: u32, encode: bool) -> usize {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if buf[i + 3] == 0xEB {
            let src = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], 0]) << 2;
            let here = now_pos.wrapping_add(i as u32).wrapping_add(8);
            let dest = if encode { src.wrapping_add(here) } else { src.wrapping_sub(here) } >> 2;
            buf[i + 2] = (dest >> 16) as u8;
            buf[i + 1] = (dest >> 8) as u8;
            buf[i] = dest as u8;
        }
        i += 4;
    }
    i
}

fn arm_thumb(buf: &mut [u8], now_pos: u32, encode: bool) -> usize {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if (buf[i + 1] & 0xF8) == 0xF0 && (buf[i + 3] & 0xF8) == 0xF8 {
            let src = ((u32::from(buf[i + 1]) & 7) << 19) | (u32::from(buf[i]) << 11) | ((u32::from(buf[i + 3]) & 7) << 8) | u32::from(buf[i + 2]);
            let src = src << 1;
            let here = now_pos.wrapping_add(i as u32).wrapping_add(4);
            let dest = if encode { src.wrapping_add(here) } else { src.wrapping_sub(here) } >> 1;
            buf[i + 1] = 0xF0 | ((dest >> 19) & 7) as u8;
            buf[i] = (dest >> 11) as u8;
            buf[i + 3] = 0xF8 | ((dest >> 8) & 7) as u8;
            buf[i + 2] = dest as u8;
            i += 2;
        }
        i += 2;
    }
    i
}

fn powerpc(buf: &mut [u8], now_pos: u32, encode: bool) -> usize {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if (buf[i] >> 2) == 0x12 && (buf[i + 3] & 3) == 1 {
            let src = ((u32::from(buf[i]) & 3) << 24) | (u32::from(buf[i + 1]) << 16) | (u32::from(buf[i + 2]) << 8) | (u32::from(buf[i + 3]) & !3);
            let here = now_pos.wrapping_add(i as u32);
            let dest = if encode { src.wrapping_add(here) } else { src.wrapping_sub(here) };
            buf[i] = 0x48 | ((dest >> 24) & 3) as u8;
            buf[i + 1] = (dest >> 16) as u8;
            buf[i + 2] = (dest >> 8) as u8;
            buf[i + 3] = (buf[i + 3] & 3) | (dest as u8 & !3);
        }
        i += 4;
    }
    i
}

fn sparc(buf: &mut [u8], now_pos: u32, encode: bool) -> usize {
    let mut i = 0;
    while i + 4 <= buf.len() {
        if (buf[i] == 0x40 && (buf[i + 1] & 0xC0) == 0x00) || (buf[i] == 0x7F && (buf[i + 1] & 0xC0) == 0xC0) {
            let src = u32::from_be_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) << 2;
            let here = now_pos.wrapping_add(i as u32);
            let dest = if encode { src.wrapping_add(here) } else { src.wrapping_sub(here) } >> 2;
            let dest = ((0u32.wrapping_sub((dest >> 22) & 1) << 22) & 0x3FFF_FFFF) | (dest & 0x3F_FFFF) | 0x4000_0000;
            buf[i..i + 4].copy_from_slice(&dest.to_be_bytes());
        }
        i += 4;
    }
    i
}

fn ia64(buf: &mut [u8], now_pos: u32, encode: bool) -> usize {
    const BRANCH_TABLE: [u8; 32] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 4, 4, 6, 6, 0, 0, 7, 7, 4, 4, 0, 0, 4, 4, 0, 0];
    let mut i = 0;
    while i + 16 <= buf.len() {
        let mask = BRANCH_TABLE[(buf[i] & 0x1F) as usize];
        let mut bit_pos = 5u32;
        for slot in 0..3 {
            if (mask >> slot) & 1 != 0 {
                let byte_pos = (bit_pos >> 3) as usize;
                let bit_res = bit_pos & 7;
                let mut instruction = 0u64;
                for j in 0..6 {
                    instruction |= u64::from(buf[i + j + byte_pos]) << (8 * j);
                }
                let mut norm = instruction >> bit_res;
                if ((norm >> 37) & 0xF) == 0x5 && ((norm >> 9) & 0x7) == 0 {
                    let mut src = ((norm >> 13) & 0xF_FFFF) as u32;
                    src |= (((norm >> 36) & 1) as u32) << 20;
                    src <<= 4;
                    let here = now_pos.wrapping_add(i as u32);
                    let dest = if encode { src.wrapping_add(here) } else { src.wrapping_sub(here) } >> 4;
                    norm &= !(0x8F_FFFFu64 << 13);
                    norm |= u64::from(dest & 0xF_FFFF) << 13;
                    norm |= u64::from(dest & 0x10_0000) << (36 - 20);
                    instruction &= (1u64 << bit_res) - 1;
                    instruction |= norm << bit_res;
                    for j in 0..6 {
                        buf[i + j + byte_pos] = (instruction >> (8 * j)) as u8;
                    }
                }
            }
            bit_pos += 41;
        }
        i += 16;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut state = 0x1234_5678u32;
        (0..6000)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                match i % 7 {
                    0 => 0xE8,
                    1 => 0xEB,
                    2 => 0x00,
                    3 => 0xF0,
                    _ => (state >> 24) as u8,
                }
            })
            .collect()
    }

    #[test]
    fn branch_filters_invert_in_pieces() {
        let data = sample();
        for id in [FILTER_X86, FILTER_POWERPC, FILTER_IA64, FILTER_ARM, FILTER_ARMTHUMB, FILTER_SPARC] {
            let mut enc = Filter::bcj(id, 0, true).unwrap();
            let mut dec = Filter::bcj(id, 0, false).unwrap();
            let mut encoded = Vec::new();
            for piece in data.chunks(97) {
                encoded.extend(enc.run(piece.to_vec(), false));
            }
            encoded.extend(enc.run(Vec::new(), true));
            assert_eq!(encoded.len(), data.len());
            let mut decoded = Vec::new();
            for piece in encoded.chunks(61) {
                decoded.extend(dec.run(piece.to_vec(), false));
            }
            decoded.extend(dec.run(Vec::new(), true));
            assert_eq!(decoded, data, "filter {id:#x}");
        }
    }

    #[test]
    fn delta_inverts() {
        let data = sample();
        let mut enc = Filter::delta(4, true);
        let mut dec = Filter::delta(4, false);
        let encoded = enc.run(data.clone(), false);
        assert_ne!(encoded, data);
        assert_eq!(dec.run(encoded, false), data);
    }
}
