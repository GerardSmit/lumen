//! Bytecode slot use shared by the VM and native front end.

use super::Op;

/// The local slots an operation reads or writes.
pub(crate) fn op_slots(op: &Op) -> Vec<u16> {
    match *op {
        Op::LoadLocal(s)
        | Op::StoreLocal(s)
        | Op::UpdateLocal(s, _)
        | Op::Tdz(s)
        | Op::GetPropLocal(s, ..)
        | Op::SetPropLocalDrop(s, ..)
        | Op::GetElemLocal(s)
        | Op::SetElemLocal(s)
        | Op::SetElemLocalDrop(s)
        | Op::ToPropKeyLocal(s)
        | Op::IterCloseL(s)
        | Op::IterAbortL(s) => vec![s],
        Op::IterStepL(a, b) | Op::IterRestL(a, b) | Op::JumpIfNotCmpLL(_, a, b, _) => vec![a, b],
        Op::JumpIfNotCmpLK(_, a, ..) => vec![a],
        Op::ArithLL(_, d, a, b) => vec![d, a, b],
        Op::ArithLK(_, d, a, _) => vec![d, a],
        Op::ForInStepL(a, b, c) => vec![a, b, c],
        _ => Vec::new(),
    }
}
