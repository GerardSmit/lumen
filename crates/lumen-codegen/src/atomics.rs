//! Sequentially consistent I32 memory intrinsics. ARM and x64 lower these imports
//! inline; other native targets call the same language-neutral operations.
use crate::{Signature, Type};
use core::sync::atomic::{AtomicU32, Ordering};

pub const ADD32: u32 = 0xffff_ff00;
pub const COMPARE_EXCHANGE32: u32 = 0xffff_ff01;

pub fn signature(id: u32) -> Option<Signature> {
    let n = match id { ADD32 => 2, COMPARE_EXCHANGE32 => 3, _ => return None };
    let mut args = vec![Type::I32; n];
    args[0] = Type::I64;
    Some(Signature::new(args, vec![Type::I32]))
}

/// The caller owns a live, aligned 4-byte location and synchronizes all other accesses.
unsafe extern "C" fn add32(pointer: *mut AtomicU32, value: u32) -> u32 {
    unsafe { (*pointer).fetch_add(value, Ordering::SeqCst) }
}
unsafe extern "C" fn compare_exchange32(pointer: *mut AtomicU32, expected: u32, replacement: u32) -> u32 {
    unsafe { (*pointer).compare_exchange(expected, replacement, Ordering::SeqCst, Ordering::SeqCst)
        .unwrap_or_else(|old| old) }
}

pub fn address(id: u32) -> Option<u64> {
    match id {
        ADD32 => Some(add32 as *const () as usize as u64),
        COMPARE_EXCHANGE32 => Some(compare_exchange32 as *const () as usize as u64),
        _ => None,
    }
}
