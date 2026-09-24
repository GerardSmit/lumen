//! Call-site source positions of a compiled chunk, for stack traces.
//!
//! Every call-site op ([`is_site`]) the compiler emits records the source position of the call
//! or `new` expression it came from ([`crate::ast::NO_POS`] when it has none), in emission order.
//! The passes that rewrite ops afterwards (`peephole`, `switch_pass`, the tail-call rewrite) never
//! drop, fuse or reorder a call-site op, so the k-th call-site op of the finished chunk is the
//! k-th recorded position: the table needs no pcs at all. It is stored as zigzag-LEB128 deltas
//! (positions mostly ascend, so most entries take one or two bytes) and decoded only when a
//! stack trace is formatted — never on the execution path. A running frame publishes the pc of
//! the call op it is executing (see `Interp::cur_site`), and [`lookup`] maps it back.

use super::Op;
use crate::ast::NO_POS;

/// Ops that call into another function on behalf of a call or `new` expression.
#[inline]
pub(crate) fn is_site(op: &Op) -> bool {
    matches!(
        op,
        Op::Call(_)
            | Op::CallWithThis(_)
            | Op::CallSpread(_)
            | Op::CallSpreadThis(_)
            | Op::New(_)
            | Op::NewSpread(_)
            | Op::SuperCall(_)
            | Op::SuperCallSpread(_)
            | Op::TailCall(..)
            | Op::TailCallSpread(..)
    )
}

fn put(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn get(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let byte = *b.get(*at)?;
        *at += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// The table for a finished chunk's `ops`, from the positions recorded per emitted call-site op.
/// Empty when nothing has a position, or (defensively) when the counts disagree — a chunk then
/// simply has no call positions rather than wrong ones.
pub(crate) fn encode(ops: &[Op], sites: &[u32]) -> Box<[u8]> {
    if sites.iter().all(|&p| p == NO_POS) || ops.iter().filter(|op| is_site(op)).count() != sites.len()
    {
        return Box::default();
    }
    let mut out = Vec::with_capacity(sites.len() * 2);
    let mut prev = -1i64;
    for &p in sites {
        let v = if p == NO_POS { -1 } else { p as i64 };
        let d = v - prev;
        put(&mut out, ((d << 1) ^ (d >> 63)) as u64);
        prev = v;
    }
    out.into_boxed_slice()
}

/// The source position of the call op at `pc` ([`NO_POS`] when `pc` is not a call-site op or
/// the table has none for it).
pub(crate) fn lookup(ops: &[Op], table: &[u8], pc: usize) -> u32 {
    if table.is_empty() || !ops.get(pc).is_some_and(is_site) {
        return NO_POS;
    }
    let k = ops[..pc].iter().filter(|op| is_site(op)).count();
    let (mut at, mut v) = (0usize, -1i64);
    for _ in 0..=k {
        let Some(z) = get(table, &mut at) else {
            return NO_POS;
        };
        v += ((z >> 1) as i64) ^ -((z & 1) as i64);
    }
    u32::try_from(v).unwrap_or(NO_POS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_round_trip_by_call_site_order() {
        let ops = [
            Op::Undef,
            Op::Call(0),
            Op::Pop,
            Op::New(1),
            Op::CallWithThis(2),
            Op::TailCall(0, false),
        ];
        let t = encode(&ops, &[40, NO_POS, 7, 1_000_000]);
        assert_eq!(lookup(&ops, &t, 1), 40);
        assert_eq!(lookup(&ops, &t, 3), NO_POS);
        assert_eq!(lookup(&ops, &t, 4), 7);
        assert_eq!(lookup(&ops, &t, 5), 1_000_000);
        assert_eq!(lookup(&ops, &t, 0), NO_POS);
        assert_eq!(lookup(&ops, &t, 99), NO_POS);
        // A count mismatch or no positions at all: no table.
        assert!(encode(&ops, &[1, 2]).is_empty());
        assert!(encode(&ops, &[NO_POS; 4]).is_empty());
    }
}
