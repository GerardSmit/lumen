//! `_sha2`: SHA-224/256/384/512 constructors over the shared digest code.

use super::native::*;
use crate::object::*;
use crate::vm::*;
use lumen_common::codec::hex_encode;
use lumen_common::hash::{Algo, Hasher};

struct Sha(Hasher);

fn sha<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&mut Hasher) -> X) -> R<X> {
    match with_opaque::<Sha, _>(v, |s| f(&mut s.0)) {
        Some(x) => Ok(x),
        None => Err(it.self_state_err("hashlib.HASH")),
    }
}

fn data_arg(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    if v.as_str().is_some() {
        return Err(it.type_error("Strings must be encoded before hashing"));
    }
    it.bytes_of(v)
}

fn construct(it: &mut Interp, cls: &Obj, algo: Algo, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("sha", a, kw, &["string", "usedforsecurity"], 0)?;
    let mut h = Hasher::new(algo);
    if let Some(data) = b[0].as_ref().filter(|v| !v.is_none()) {
        let bytes = data_arg(it, data)?;
        h.update(&bytes);
    }
    Ok(new_opaque(cls, Sha(h)))
}

fn update(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("update", a, 2, 2)?;
    let bytes = data_arg(it, &a[1])?;
    sha(it, &a[0], |h| h.update(&bytes))?;
    Ok(Value::None)
}

fn finished(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    sha(it, v, |h| h.clone().finish())
}

fn digest(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("digest", a, 1, 1)?;
    Ok(Value::bytes(finished(it, &a[0])?))
}

fn hexdigest(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("hexdigest", a, 1, 1)?;
    Ok(Value::str(&hex_encode(&finished(it, &a[0])?)))
}

fn copy(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("copy", a, 1, 1)?;
    let h = sha(it, &a[0], |h| h.clone())?;
    let cls = it.type_of(&a[0]);
    Ok(new_opaque(&cls, Sha(h)))
}

fn algo_of(it: &mut Interp, v: &Value) -> R<Algo> {
    sha(it, v, |h| h.algo())
}

fn name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("name", a, 1, 1)?;
    let n = match algo_of(it, &a[0])? {
        Algo::Sha224 => "sha224",
        Algo::Sha256 => "sha256",
        Algo::Sha384 => "sha384",
        _ => "sha512",
    };
    Ok(Value::str(n))
}

fn digest_size(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("digest_size", a, 1, 1)?;
    Ok(Value::Int(algo_of(it, &a[0])?.out_len() as i64))
}

fn block_size(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("block_size", a, 1, 1)?;
    Ok(Value::Int(algo_of(it, &a[0])?.block_len() as i64))
}

fn type_in_module(it: &mut Interp, name: &str) -> Obj {
    let Some(Value::Obj(m)) = dict_get_str(&it.modules, "_sha2") else { unreachable!("_sha2 constructors exist only inside the module") };
    let d = it.module_dict(&m);
    match dict_get_str(&d, name) {
        Some(Value::Obj(t)) => t,
        _ => unreachable!("type registered by make"),
    }
}

macro_rules! constructor {
    ($fname:ident, $algo:expr, $cls:literal) => {
        fn $fname(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
            let cls = type_in_module(it, $cls);
            construct(it, &cls, $algo, a, kw)
        }
    };
}

constructor!(sha224, Algo::Sha224, "SHA224Type");
constructor!(sha256, Algo::Sha256, "SHA256Type");
constructor!(sha384, Algo::Sha384, "SHA384Type");
constructor!(sha512, Algo::Sha512, "SHA512Type");

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_sha2");
    let d = it.module_dict(&m);
    it.register_module("_sha2", &m);
    let ctors: [(&'static str, &'static str, NativeFn); 4] =
        [("sha224", "SHA224Type", sha224), ("sha256", "SHA256Type", sha256), ("sha384", "SHA384Type", sha384), ("sha512", "SHA512Type", sha512)];
    for (fname, tname, f) in ctors {
        let ty = new_type(it, "_sha2", tname, None, Layout::Other);
        let methods: &[(&'static str, NativeFn)] = &[("update", update), ("digest", digest), ("hexdigest", hexdigest), ("copy", copy)];
        for (n, mf) in methods {
            it.reg(&ty, n, *mf);
        }
        let props: &[(&'static str, NativeFn)] = &[("name", name), ("digest_size", digest_size), ("block_size", block_size)];
        for (n, pf) in props {
            it.reg_prop(&ty, n, *pf);
        }
        set_type(&d, tname, &ty);
        set_fn(it, &d, fname, f);
    }
    m
}
