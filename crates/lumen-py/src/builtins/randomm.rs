//! `_random`: the Mersenne Twister generator behind `random.Random`.

use super::native::*;
use crate::object::*;
use crate::pyint::BigInt;
use crate::vm::*;
use lumen_common::mt19937::{Mt19937, N};

struct RandomState(Mt19937);

fn rs<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&mut Mt19937) -> X) -> R<X> {
    match with_opaque::<RandomState, _>(v, |s| f(&mut s.0)) {
        Some(x) => Ok(x),
        None => Err(it.self_state_err("_random.Random")),
    }
}

fn seed_with(it: &mut Interp, target: &Value, arg: &Value) -> R<()> {
    let key: Vec<u32> = if arg.is_none() {
        let mut buf = [0u8; N * 4];
        it.platform.borrow_mut().entropy(&mut buf);
        buf.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
    } else {
        let magnitude = match arg.as_bigint() {
            Some(b) => b.abs(),
            None => {
                let h = it.hash_value(arg)?;
                BigInt::from_u64(h as u64)
            }
        };
        let words = magnitude.words().1;
        let mut key: Vec<u32> = words.iter().flat_map(|w| [*w as u32, (*w >> 32) as u32]).collect();
        while key.len() > 1 && key.last() == Some(&0) {
            key.pop();
        }
        if key.is_empty() {
            key.push(0);
        }
        key
    };
    rs(it, target, |mt| mt.init_by_array(&key))
}

fn random_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let Some(Value::Obj(cls)) = a.first() else {
        return Err(it.type_error("Random.__new__(X): X is not a type object"));
    };
    if !kw.is_empty() {
        return Err(it.type_error("Random() takes no keyword arguments"));
    }
    if a.len() > 2 {
        return Err(it.type_error("Random() requires 0 or 1 argument"));
    }
    let obj = new_opaque(cls, RandomState(Mt19937::new()));
    let arg = a.get(1).cloned().unwrap_or(Value::None);
    seed_with(it, &obj, &arg)?;
    Ok(obj)
}

fn random_seed(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("seed", a, 1, 2)?;
    let arg = a.get(1).cloned().unwrap_or(Value::None);
    seed_with(it, &a[0], &arg)?;
    Ok(Value::None)
}

fn random_random(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("random", a, 1, 1)?;
    Ok(Value::Float(rs(it, &a[0], |mt| mt.next_f64())?))
}

fn random_getrandbits(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("getrandbits", a, 2, 2)?;
    if !it.has_index(&a[1]) {
        let t = it.type_name_of(&a[1]);
        return Err(it.type_error(&format!("'{}' object cannot be interpreted as an integer", t)));
    }
    let k = it.index_of(&a[1])?;
    if k < 0 {
        return Err(it.value_error("number of bits must be non-negative"));
    }
    if k == 0 {
        return Ok(Value::Int(0));
    }
    let words = rs(it, &a[0], |mt| mt.random_bits(k as u64))?;
    if words.len() == 1 {
        return Ok(Value::Int(words[0] as i64));
    }
    let wide: Vec<u64> = words.chunks(2).map(|c| c[0] as u64 | (c.get(1).copied().unwrap_or(0) as u64) << 32).collect();
    Ok(Value::big(BigInt::from_words(false, wide)))
}

fn random_getstate(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("getstate", a, 1, 1)?;
    let items = rs(it, &a[0], |mt| {
        let (state, index) = mt.state();
        state.iter().map(|w| Value::Int(*w as i64)).chain(std::iter::once(Value::Int(index as i64))).collect::<Vec<_>>()
    })?;
    Ok(Value::tuple(items))
}

fn random_setstate(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("setstate", a, 2, 2)?;
    let Some(items) = a[1].tuple_items() else {
        return Err(it.type_error("state vector must be a tuple"));
    };
    if items.len() != N + 1 {
        return Err(it.value_error("state vector is the wrong size"));
    }
    let mut state = [0u32; N];
    for (slot, item) in state.iter_mut().zip(items) {
        let Some(b) = item.as_bigint() else {
            return Err(it.type_error("an integer is required"));
        };
        if b.is_negative() {
            return Err(it.overflow_err("can't convert negative value to unsigned int"));
        }
        match b.words().1 {
            [] => *slot = 0,
            [w] => *slot = *w as u32,
            _ => return Err(it.overflow_err("Python int too large to convert to C unsigned long")),
        }
    }
    let Some(index) = items[N].as_bigint() else {
        return Err(it.type_error("an integer is required"));
    };
    let Some(index) = index.to_i64() else {
        return Err(it.overflow_err("Python int too large to convert to C long"));
    };
    if !(0..=N as i64).contains(&index) {
        return Err(it.value_error("invalid state"));
    }
    rs(it, &a[0], |mt| mt.set_state(state, index as usize))?;
    Ok(Value::None)
}

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_random");
    let d = it.module_dict(&m);
    it.register_module("_random", &m);

    let ty = new_type(it, "_random", "Random", None, Layout::Other);
    it.reg_new(&ty, random_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("seed", random_seed),
        ("random", random_random),
        ("getrandbits", random_getrandbits),
        ("getstate", random_getstate),
        ("setstate", random_setstate),
    ];
    for (n, f) in methods {
        it.reg(&ty, n, *f);
    }
    set_type(&d, "Random", &ty);
    m
}
