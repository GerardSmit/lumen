//! Unicode normalization (NFC/NFD/NFKC/NFKD) on code points (UAX #15). The algorithm is generic
//! over a [`NormData`] table set: [`Latest`] is the `unicode_norm` data the JS engine uses, and
//! `ucd` supplies Python's pinned database. Hangul syllables decompose/compose algorithmically.

use crate::unicode_norm::{CANON_DECOMP, CCC, COMPAT_DECOMP, COMPOSE};

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// The character data normalization reads.
pub trait NormData {
    /// The canonical combining class of `cp` (0 for starters).
    fn ccc(&self, cp: u32) -> u8;
    /// Push `cp`'s decomposition mapping (canonical, or any with `compat`) onto `stack` in
    /// reverse order, without applying it recursively; false when it has none.
    fn push_mapping(&self, cp: u32, compat: bool, stack: &mut Vec<u32>) -> bool;
    /// The primary composite of the pair `(a, b)`, Hangul aside.
    fn compose(&self, a: u32, b: u32) -> Option<u32>;
}

/// The `unicode_norm` tables (the newest Unicode version Lumen ships).
pub struct Latest;

impl NormData for Latest {
    fn ccc(&self, cp: u32) -> u8 {
        match CCC.binary_search_by_key(&cp, |&(c, _)| c) {
            Ok(k) => CCC[k].1,
            Err(_) => 0,
        }
    }

    fn push_mapping(&self, cp: u32, compat: bool, stack: &mut Vec<u32>) -> bool {
        if let Ok(k) = CANON_DECOMP.binary_search_by_key(&cp, |&(c, _, _)| c) {
            let (_, a, b) = CANON_DECOMP[k];
            if b != 0 {
                stack.push(b);
            }
            stack.push(a);
            return true;
        }
        if compat {
            // COMPAT_DECOMP is already fully NFKD-expanded; expanding it again is a no-op.
            if let Ok(k) = COMPAT_DECOMP.binary_search_by_key(&cp, |&(c, _)| c) {
                stack.extend(COMPAT_DECOMP[k].1.iter().rev());
                return true;
            }
        }
        false
    }

    fn compose(&self, a: u32, b: u32) -> Option<u32> {
        COMPOSE.binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b))).ok().map(|k| COMPOSE[k].2)
    }
}

/// The canonical combining class of `cp` (0 for starters).
pub fn ccc(cp: u32) -> u8 {
    Latest.ccc(cp)
}

fn compose_pair<D: NormData + ?Sized>(data: &D, a: u32, b: u32) -> Option<u32> {
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        return Some(S_BASE + ((a - L_BASE) * V_COUNT + (b - V_BASE)) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a)
        && (a - S_BASE).is_multiple_of(T_COUNT)
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&b)
    {
        return Some(a + (b - T_BASE));
    }
    data.compose(a, b)
}

/// Fully decompose `cp` onto `out`.
fn push_decomp<D: NormData + ?Sized>(data: &D, cp: u32, compat: bool, stack: &mut Vec<u32>, out: &mut Vec<u32>) {
    stack.push(cp);
    while let Some(cp) = stack.pop() {
        if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
            let s = cp - S_BASE;
            out.push(L_BASE + s / N_COUNT);
            out.push(V_BASE + (s % N_COUNT) / T_COUNT);
            if !s.is_multiple_of(T_COUNT) {
                out.push(T_BASE + s % T_COUNT);
            }
        } else if !data.push_mapping(cp, compat, stack) {
            out.push(cp);
        }
    }
}

/// Canonical ordering: stable-sort sequences of nonzero-class code points by combining class.
fn canonical_order<D: NormData + ?Sized>(data: &D, cps: &mut [u32]) {
    let mut i = 1;
    while i < cps.len() {
        let cc = data.ccc(cps[i]);
        if cc != 0 {
            let mut j = i;
            while j > 0 && data.ccc(cps[j - 1]) > cc {
                cps.swap(j - 1, j);
                j -= 1;
            }
        }
        i += 1;
    }
}

/// NFD (or NFKD with `compat`) of a code-point sequence over `data`.
pub fn decompose_with<D: NormData + ?Sized>(data: &D, cps: &[u32], compat: bool) -> Vec<u32> {
    let mut out = Vec::with_capacity(cps.len());
    let mut stack = Vec::new();
    for &cp in cps {
        push_decomp(data, cp, compat, &mut stack, &mut out);
    }
    canonical_order(data, &mut out);
    out
}

/// Canonical composition (UAX #15): recombine each starter with following unblocked marks
/// (a mark is blocked when a character of equal-or-higher class — or another starter — sits
/// between it and the starter).
pub fn compose_with<D: NormData + ?Sized>(data: &D, cps: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::with_capacity(cps.len());
    let mut starter: Option<usize> = None;
    let mut last_cc: i32 = -1;
    for &cp in cps {
        let cc = data.ccc(cp) as i32;
        if let Some(si) = starter {
            if last_cc < cc {
                if let Some(c) = compose_pair(data, out[si], cp) {
                    out[si] = c;
                    continue;
                }
            }
        }
        out.push(cp);
        if cc == 0 {
            starter = Some(out.len() - 1);
            last_cc = -1;
        } else {
            last_cc = cc;
        }
    }
    out
}

/// Normalize `cps` over `data` to the requested form ("NFC" | "NFD" | "NFKC" | "NFKD").
pub fn normalize_with<D: NormData + ?Sized>(data: &D, cps: &[u32], form: &str) -> Vec<u32> {
    let d = decompose_with(data, cps, form.starts_with("NFK"));
    if form.ends_with('C') {
        compose_with(data, &d)
    } else {
        d
    }
}

/// NFD (or NFKD with `compat`) of a code-point sequence.
pub fn decompose(cps: &[u32], compat: bool) -> Vec<u32> {
    decompose_with(&Latest, cps, compat)
}

/// Canonical composition over the [`Latest`] tables.
pub fn compose(cps: &[u32]) -> Vec<u32> {
    compose_with(&Latest, cps)
}

/// Normalize `cps` to the requested form ("NFC" | "NFD" | "NFKC" | "NFKD").
pub fn normalize(cps: &[u32], form: &str) -> Vec<u32> {
    normalize_with(&Latest, cps, form)
}
