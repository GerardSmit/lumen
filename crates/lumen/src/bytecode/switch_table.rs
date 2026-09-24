//! `switch` dispatch tables: a chain of case tests on one local
//!
//! ```text
//!   p+0  JumpIfNotCmpLK(StrictEq, s, k0, p+2)    ; case k0?
//!   p+1  Jump(body0)
//!   p+2  JumpIfNotCmpLK(StrictEq, s, k1, p+4)    ; case k1?
//!   p+3  Jump(body1)
//!   …
//! ```
//!
//! (what `switch` lowers to, and what `if (x === K) … else if (x === K2) …` chains become after
//! the peephole) costs one compare-and-branch per case tried. [`switch_pass`] puts an
//! [`Op::SwitchLK`] in front of each such chain; on its first execution the op builds a
//! [`SwitchTable`] from the chain's constants and from then on jumps straight to the matching
//! case body — or past the chain's covered prefix on a miss. The chain itself stays in place
//! and stays the reference semantics: the table only covers Number and String constants (strict
//! equality on them is key equality), and anything it can't decide — a slot in its TDZ — falls
//! through into the chain unchanged.

use super::{CmpKind, Op};
use crate::fasthash::FastMap;
use crate::lstr::LStr;
use crate::value::Value;

/// Shortest chain worth a table (shorter ones stay plain compare-and-branch sequences).
const MIN_CASES: usize = 4;

/// Dense Number tables cover integer keys below this bound.
const DENSE_MAX: usize = 4096;

pub(super) struct SwitchTable {
    /// Integer keys `0..dense.len()`: the case target, or `u32::MAX` for none.
    dense: Vec<u32>,
    /// Other Number keys by `f64` bits (`-0` folded into `+0`; NaN never matches).
    nums: FastMap<u64, u32>,
    strs: FastMap<LStr, u32>,
    /// Where a value matching none of the covered cases continues: the first op after the
    /// covered prefix of the chain.
    end: u32,
}

/// The chain of `(slot, first pc)` that starts at `pc`, and its length in cases.
fn chain_at(ops: &[Op], pc: usize) -> Option<(u16, usize)> {
    let mut slot = None;
    let mut p = pc;
    let mut n = 0;
    while p + 1 < ops.len() {
        match (ops[p], ops[p + 1]) {
            (Op::JumpIfNotCmpLK(CmpKind::StrictEq, s, _, t), Op::Jump(_))
                if t as usize == p + 2 && slot.is_none_or(|x| x == s) =>
            {
                slot = Some(s);
                n += 1;
                p += 2;
            }
            _ => break,
        }
    }
    Some((slot?, n))
}

/// Insert an [`Op::SwitchLK`] before every case chain of at least [`MIN_CASES`] tests (jumps to
/// the chain's head now reach the table op). Returns how many tables the chunk needs.
pub(super) fn switch_pass(ops: &mut Vec<Op>) -> usize {
    let n = ops.len();
    let mut out = Vec::with_capacity(n);
    let mut map = vec![0u32; n + 1];
    let mut tables = 0u32;
    let mut pc = 0;
    while pc < n {
        map[pc] = out.len() as u32;
        if let Some((slot, cases)) = chain_at(ops, pc) {
            if cases >= MIN_CASES {
                out.push(Op::SwitchLK(slot, tables));
                tables += 1;
            }
            // Copy the whole chain (its interior heads are not table entry points).
            for q in pc..pc + 2 * cases {
                map[q] = out.len() as u32;
                out.push(ops[q]);
            }
            if cases > 0 {
                pc += 2 * cases;
                continue;
            }
        }
        out.push(ops[pc]);
        pc += 1;
    }
    map[n] = out.len() as u32;
    if tables == 0 {
        return 0;
    }
    super::remap_targets(&mut out, &map);
    *ops = out;
    tables as usize
}

impl SwitchTable {
    /// Build from the chain following the `SwitchLK` at `pc - 1` (post-pass pcs). The covered
    /// prefix stops at the first case constant that is neither a Number nor a String.
    pub(super) fn build(ops: &[Op], pc: usize, slot: u16, consts: &[Value]) -> SwitchTable {
        let mut t = SwitchTable {
            dense: Vec::new(),
            nums: FastMap::default(),
            strs: FastMap::default(),
            end: pc as u32,
        };
        let mut ints: Vec<(usize, u32)> = Vec::new();
        let mut p = pc;
        while p + 1 < ops.len() {
            let (Op::JumpIfNotCmpLK(CmpKind::StrictEq, s, k, next), Op::Jump(body)) =
                (ops[p], ops[p + 1])
            else {
                break;
            };
            if s != slot || next as usize != p + 2 {
                break;
            }
            match &consts[k as usize] {
                Value::Num(x) => {
                    let x = if *x == 0.0 { 0.0 } else { *x };
                    if x.is_nan() {
                        // Never strictly equal to anything: the case can't match.
                    } else if x >= 0.0 && x < DENSE_MAX as f64 && x.fract() == 0.0 {
                        ints.push((x as usize, body));
                    } else {
                        t.nums.entry(x.to_bits()).or_insert(body);
                    }
                }
                Value::Str(s) => {
                    t.strs.entry(s.clone()).or_insert(body);
                }
                _ => break,
            }
            t.end = next;
            p += 2;
        }
        if let Some(max) = ints.iter().map(|&(k, _)| k).max() {
            t.dense = vec![u32::MAX; max + 1];
            // First case in chain order wins (duplicate labels are legal).
            for &(k, body) in &ints {
                if t.dense[k] == u32::MAX {
                    t.dense[k] = body;
                }
            }
        }
        t
    }

    /// Where execution continues for discriminant `v`, or `None` to run the chain as-is.
    #[inline]
    pub(super) fn target(&self, v: &Value) -> Option<u32> {
        Some(match v {
            Value::Num(x) => {
                let x = *x;
                let hit = if x >= 0.0 && x < self.dense.len() as f64 && x.fract() == 0.0 {
                    self.dense[x as usize]
                } else if self.nums.is_empty() || x.is_nan() {
                    u32::MAX
                } else {
                    let x = if x == 0.0 { 0.0 } else { x };
                    self.nums.get(&x.to_bits()).copied().unwrap_or(u32::MAX)
                };
                if hit == u32::MAX {
                    self.end
                } else {
                    hit
                }
            }
            Value::Str(s) => {
                if self.strs.is_empty() {
                    self.end
                } else {
                    self.strs.get(s).copied().unwrap_or(self.end)
                }
            }
            // TDZ: the chain's first test throws the ReferenceError.
            Value::Empty => return None,
            // No covered case constant is strictly equal to any other kind of value.
            _ => self.end,
        })
    }
}
