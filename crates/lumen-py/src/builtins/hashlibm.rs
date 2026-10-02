//! `_hashlib`, `_md5`, `_sha1`, `_sha2`, `_sha3` and `_blake2`: Python's digest objects over the
//! shared digests, HMAC, PBKDF2 and scrypt in `lumen_common::hash`.

use crate::bind::Py;
use crate::object::*;
use crate::vm::Interp;
use lumen_common::codec::hex_encode;
use lumen_common::hash::{Algo, Blake2, Hasher};

/// The bytes of a buffer argument; `str` gets hashlib's own error.
fn data_of(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    if v.as_str().is_some() {
        return Err(it.type_error("Strings must be encoded before hashing"));
    }
    if !it.is_buffer(v) {
        return Err(it.type_error("object supporting the buffer API required"));
    }
    it.bytes_of(v)
}

/// The initial data of a constructor, given as `data` or as the deprecated `string` keyword.
fn initial_data(it: &mut Interp, data: Option<&Value>, string: Option<&Value>) -> R<Option<Vec<u8>>> {
    match (data, string) {
        (Some(_), Some(_)) => Err(it.type_error(
            "'data' and 'string' are mutually exclusive and support for 'string' keyword parameter is slated for removal in a future version.",
        )),
        (Some(v), None) | (None, Some(v)) => data_of(it, v).map(Some),
        (None, None) => Ok(None),
    }
}

/// The digests by their Python names, as `hashlib` and `_hashlib.new` spell them.
const NAMES: [(&str, Algo); 19] = [
    ("md5", Algo::Md5),
    ("sha1", Algo::Sha1),
    ("sha224", Algo::Sha224),
    ("sha256", Algo::Sha256),
    ("sha384", Algo::Sha384),
    ("sha512", Algo::Sha512),
    ("sha512_224", Algo::Sha512_224),
    ("sha512_256", Algo::Sha512_256),
    ("sha3_224", Algo::Sha3_224),
    ("sha3_256", Algo::Sha3_256),
    ("sha3_384", Algo::Sha3_384),
    ("sha3_512", Algo::Sha3_512),
    ("shake_128", Algo::Shake128),
    ("shake_256", Algo::Shake256),
    ("blake2b", Algo::Blake2b512),
    ("blake2s", Algo::Blake2s256),
    ("ripemd160", Algo::Ripemd160),
    ("sm3", Algo::Sm3),
    ("md5-sha1", Algo::Md5Sha1),
];

fn algo_by_name(name: &str) -> Option<Algo> {
    let lower = name.to_ascii_lowercase();
    NAMES.iter().find(|(n, _)| *n == lower).map(|&(_, a)| a).or_else(|| Algo::from_name(name))
}

fn py_name(algo: Algo) -> &'static str {
    NAMES.iter().find(|&&(_, a)| a == algo).map_or("unknown", |&(n, _)| n)
}

fn new_hash(it: &mut Interp, algo: Algo, data: Option<Vec<u8>>) -> Value {
    let mut h = Hasher::new(algo);
    if let Some(d) = data {
        h.update(&d);
    }
    Py::new(it, _hashlib::Hash { h }).value().clone()
}

/// A constructor of one fixed digest: `data` (or `string`) is the initial input.
fn construct(it: &mut Interp, algo: Algo, data: Option<&Value>, string: Option<&Value>) -> R<Value> {
    let data = initial_data(it, data, string)?;
    Ok(new_hash(it, algo, data))
}

/// `_hashlib` (OpenSSL in CPython).
/// `hmac.compare_digest` (also `_operator._compare_digest`): constant-time equality of two ASCII
/// strs or two bytes-like objects.
pub fn compare_digest(it: &mut Interp, a: &Value, b: &Value) -> R<bool> {
    let (x, y) = match (a.as_str(), b.as_str()) {
        (Some(x), Some(y)) => {
            if !x.is_ascii() || !y.is_ascii() {
                return Err(it.type_error("comparing strings with non-ASCII characters is not supported"));
            }
            (x.as_bytes().to_vec(), y.as_bytes().to_vec())
        }
        _ if !it.is_buffer(a) || !it.is_buffer(b) || a.as_str().is_some() || b.as_str().is_some() => {
            let (ta, tb) = (it.type_name_of(a), it.type_name_of(b));
            return Err(it.type_error(&format!("unsupported operand types(s) or combination of types: '{ta}' and '{tb}'")));
        }
        _ => (it.buffer_bytes(a)?, it.buffer_bytes(b)?),
    };
    Ok(lumen_common::hash::constant_time_eq(&x, &y))
}

#[lumen_bind::module(name = "_hashlib")]
pub mod _hashlib {
    use super::*;
    use crate::bind::{type_object, This};
    use crate::vm::dict_set_str;
    use lumen_common::hash::Hmac;

    #[derive(Default)]
    pub struct State {
        unsupported: Option<Obj>,
    }

    fn unsupported(it: &mut Interp, msg: String) -> Obj {
        let cls = match it.native_state::<State>().unsupported.clone() {
            Some(c) => c,
            None => it.exc_type("ValueError"),
        };
        it.new_exc(&cls, vec![Value::string(msg)])
    }

    /// The digest an HMAC or KDF names: a name or one of the `openssl_*` constructors.
    fn mac_algo(it: &mut Interp, digest: &Value) -> R<Algo> {
        let name = match digest.as_str() {
            Some(s) => s.to_string(),
            None => {
                let n = it.get_attr_str(digest, "__name__").ok();
                match n.as_ref().and_then(|n| n.as_str()).and_then(|n| n.strip_prefix("openssl_")) {
                    Some(n) if it.is_callable(digest) => n.to_string(),
                    _ => {
                        let r = it.repr_of(digest)?;
                        return Err(unsupported(it, format!("Unsupported digestmod {r}")));
                    }
                }
            }
        };
        match algo_by_name(&name).filter(|&a| lumen_common::hash::supports_mac(a)) {
            Some(a) => Ok(a),
            None => Err(unsupported(it, format!("unsupported hash type {name}"))),
        }
    }

    /// A hash object.
    #[class(name = "HASH", module = "_hashlib", hint(py(final)))]
    pub struct Hash {
        pub(super) h: Hasher,
    }

    impl Hash {
        fn result(&self, it: &mut Interp, method: &str, length: Option<&Value>) -> R<Vec<u8>> {
            let algo = self.h.algo();
            match (algo.is_xof(), length) {
                (true, Some(n)) => {
                    let n = it.index_of(n)?;
                    if n < 0 {
                        return Err(it.value_error("length must be non-negative"));
                    }
                    Ok(self.h.clone().finish_len(n as usize))
                }
                (true, None) => Err(it.type_error(&format!("{method}() missing required argument 'length' (pos 1)"))),
                (false, Some(_)) => Err(it.type_error(&format!("{method}() takes no arguments (1 given)"))),
                (false, None) => Ok(self.h.clone().finish()),
            }
        }
    }

    #[methods]
    impl Hash {
        /// Update this hash object's state with the provided string.
        fn update(slf: This<Py<Self>>, it: &mut Interp, obj: &Value) -> R<()> {
            let data = data_of(it, obj)?;
            slf.0.with(it, |s| s.h.update(&data))
        }

        /// Return the digest value as a bytes object.
        fn digest(slf: This<Py<Self>>, it: &mut Interp, #[kw] length: Option<&Value>) -> R<Value> {
            let h = Hash { h: slf.0.borrow(it)?.h.clone() };
            Ok(Value::bytes(h.result(it, "digest", length)?))
        }

        /// Return the digest value as a string of hexadecimal digits.
        fn hexdigest(slf: This<Py<Self>>, it: &mut Interp, #[kw] length: Option<&Value>) -> R<String> {
            let h = Hash { h: slf.0.borrow(it)?.h.clone() };
            Ok(hex_encode(&h.result(it, "hexdigest", length)?))
        }

        /// Return a copy of the hash object.
        fn copy(&self) -> Hash {
            Hash { h: self.h.clone() }
        }

        #[getter]
        fn name(&self) -> &'static str {
            py_name(self.h.algo())
        }

        #[getter]
        fn digest_size(&self) -> usize {
            let a = self.h.algo();
            if a.is_xof() {
                0
            } else {
                a.out_len()
            }
        }

        #[getter]
        fn block_size(&self) -> usize {
            self.h.algo().block_len()
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let algo = slf.0.borrow(it)?.h.algo();
            let ty = if algo.is_xof() { "HASHXOF" } else { "HASH" };
            Ok(format!("<{} _hashlib.{ty} object @ {:#x}>", py_name(algo), it.id_of(slf.0.value())))
        }
    }

    /// The object used to calculate HMAC of a message.
    #[class(name = "HMAC", module = "_hashlib", hint(py(final)))]
    pub struct HmacObj {
        m: Hmac,
        algo: Algo,
    }

    #[methods]
    impl HmacObj {
        /// Update the HMAC object with msg.
        fn update(slf: This<Py<Self>>, it: &mut Interp, #[kw] msg: &Value) -> R<()> {
            let data = data_of(it, msg)?;
            slf.0.with(it, |s| s.m.update(&data))
        }

        /// Return the digest of the bytes passed to the update() method so far.
        fn digest(&self) -> Vec<u8> {
            self.m.clone().finish()
        }

        /// Return hexadecimal digest of the bytes passed to the update() method so far.
        ///
        /// This may be used to exchange the value safely in email or other non-binary
        /// environments.
        fn hexdigest(&self) -> String {
            hex_encode(&self.m.clone().finish())
        }

        /// Return a copy ("clone") of the HMAC object.
        fn copy(&self) -> HmacObj {
            HmacObj { m: self.m.clone(), algo: self.algo }
        }

        #[getter]
        fn name(&self) -> String {
            format!("hmac-{}", py_name(self.algo))
        }

        #[getter]
        fn digest_size(&self) -> usize {
            self.algo.out_len()
        }

        #[getter]
        fn block_size(&self) -> usize {
            self.algo.block_len()
        }

        #[proto(repr)]
        fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
            let algo = slf.0.borrow(it)?.algo;
            Ok(format!("<hmac-{} HMAC object @ {:#x}>", py_name(algo), it.id_of(slf.0.value())))
        }
    }

    /// Return a new hash object using the named algorithm.
    ///
    /// An optional string argument may be provided and will be
    /// automatically hashed.
    ///
    /// The MD5 and SHA1 algorithms are always supported.
    #[op]
    fn new(
        it: &mut Interp,
        #[kw] name: &str,
        #[kw] data: Option<&Value>,
        #[kwonly] #[default(true)] usedforsecurity: bool,
        #[kwonly] string: Option<&Value>,
    ) -> R<Value> {
        let _ = usedforsecurity;
        let Some(algo) = algo_by_name(name) else {
            return Err(unsupported(it, format!("unsupported hash type {name}")));
        };
        construct(it, algo, data, string)
    }

    /// Returns a md5 hash object; optionally initialized with a string
    #[op]
    fn openssl_md5(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Md5, data, string)
    }

    /// Returns a sha1 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha1(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha1, data, string)
    }

    /// Returns a sha224 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha224(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha224, data, string)
    }

    /// Returns a sha256 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha256(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha256, data, string)
    }

    /// Returns a sha384 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha384(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha384, data, string)
    }

    /// Returns a sha512 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha512(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha512, data, string)
    }

    /// Returns a sha3-224 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha3_224(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha3_224, data, string)
    }

    /// Returns a sha3-256 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha3_256(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha3_256, data, string)
    }

    /// Returns a sha3-384 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha3_384(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha3_384, data, string)
    }

    /// Returns a sha3-512 hash object; optionally initialized with a string
    #[op]
    fn openssl_sha3_512(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha3_512, data, string)
    }

    /// Returns a shake-128 variable hash object; optionally initialized with a string
    #[op]
    fn openssl_shake_128(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Shake128, data, string)
    }

    /// Returns a shake-256 variable hash object; optionally initialized with a string
    #[op]
    fn openssl_shake_256(it: &mut Interp, #[kw] data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool, #[kwonly] string: Option<&Value>) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Shake256, data, string)
    }

    /// Password based key derivation function 2 (PKCS #5 v2.0) with HMAC as pseudorandom function.
    #[op]
    fn pbkdf2_hmac(it: &mut Interp, #[kw] hash_name: &str, #[kw] password: &Value, #[kw] salt: &Value, #[kw] iterations: i64, #[kw] dklen: Option<&Value>) -> R<Vec<u8>> {
        let password = data_of(it, password)?;
        let salt = data_of(it, salt)?;
        let algo = match algo_by_name(hash_name).filter(|&a| lumen_common::hash::supports_mac(a)) {
            Some(a) => a,
            None => return Err(unsupported(it, "[digital envelope routines] unsupported".into())),
        };
        if iterations < 1 {
            return Err(it.value_error("iteration value must be greater than 0."));
        }
        if iterations > i32::MAX as i64 {
            return Err(it.new_exc_str("OverflowError", "iteration value is too great."));
        }
        let dklen = match dklen {
            None | Some(Value::None) => algo.out_len() as i64,
            Some(v) => it.index_of(v)?,
        };
        if dklen < 1 {
            return Err(it.value_error("key length must be greater than 0."));
        }
        if dklen > i32::MAX as i64 {
            return Err(it.new_exc_str("OverflowError", "key length is too great."));
        }
        Ok(lumen_common::hash::pbkdf2(algo, &password, &salt, iterations as u32, dklen as usize))
    }

    /// scrypt password-based key derivation function.
    #[op]
    #[allow(clippy::too_many_arguments)]
    fn scrypt(
        it: &mut Interp,
        password: &Value,
        #[kwonly] salt: Option<&Value>,
        #[kwonly] n: Option<&Value>,
        #[kwonly] r: Option<&Value>,
        #[kwonly] p: Option<&Value>,
        #[kwonly] #[default(0)] maxmem: i64,
        #[kwonly] #[default(64)] dklen: i64,
    ) -> R<Vec<u8>> {
        const MAX_MEM: u64 = 32 * 1024 * 1024;
        let password = data_of(it, password)?;
        let Some(salt) = salt else { return Err(it.type_error("salt is required")) };
        let salt = data_of(it, salt)?;
        let (Some(n), Some(r), Some(p)) = (n, r, p) else {
            let missing = if n.is_none() { "n" } else if r.is_none() { "r" } else { "p" };
            return Err(it.type_error(&format!("{missing} is required and must be an unsigned int")));
        };
        let n = it.index_of(n)?;
        let r = it.index_of(r)?;
        let p = it.index_of(p)?;
        if n < 2 || n & (n - 1) != 0 {
            return Err(it.value_error("n must be a power of 2."));
        }
        if !(0..=i32::MAX as i64).contains(&maxmem) {
            return Err(it.value_error("maxmem must be positive and smaller than 2147483647"));
        }
        if !(1..=i32::MAX as i64).contains(&dklen) {
            return Err(it.value_error("dklen must be greater than 0 and smaller than 2147483647"));
        }
        let limit = if maxmem == 0 { MAX_MEM } else { maxmem as u64 };
        let need = (128u64.saturating_mul(r.max(0) as u64)).saturating_mul((n as u64).saturating_add(2).saturating_add(p.max(0) as u64));
        if r < 1 || p < 1 || r > u32::MAX as i64 || p > u32::MAX as i64 {
            return Err(it.value_error("Invalid parameter combination for n, r, p, maxmem."));
        }
        if need > limit {
            return Err(it.value_error("[digital envelope routines] memory limit exceeded"));
        }
        lumen_common::hash::scrypt(&password, &salt, n as u64, r as u32, p as u32, dklen as usize).map_err(|e| it.value_error(&e))
    }

    /// Single-shot HMAC.
    #[op]
    fn hmac_digest(it: &mut Interp, #[kw] key: &Value, #[kw] msg: &Value, #[kw] digest: &Value) -> R<Vec<u8>> {
        let key = it.bytes_of(key)?;
        let msg = data_of(it, msg)?;
        let algo = mac_algo(it, digest)?;
        Ok(lumen_common::hash::hmac(algo, &key, &msg))
    }

    /// Return a new hmac object.
    #[op]
    fn hmac_new(it: &mut Interp, #[kw] key: &Value, #[kw] msg: Option<&Value>, #[kw] digestmod: Option<&Value>) -> R<Value> {
        let key = it.bytes_of(key)?;
        let Some(digestmod) = digestmod.filter(|v| !v.is_none()) else {
            return Err(it.type_error("Missing required parameter 'digestmod'."));
        };
        let algo = mac_algo(it, digestmod)?;
        let Some(mut m) = Hmac::new(algo, &key) else {
            return Err(unsupported(it, format!("unsupported hash type {}", py_name(algo))));
        };
        if let Some(msg) = msg.filter(|v| !v.is_none()) {
            let data = data_of(it, msg)?;
            m.update(&data);
        }
        Ok(Py::new(it, HmacObj { m, algo }).value().clone())
    }

    /// Return 'a == b'.
    ///
    /// This function uses an approach designed to prevent
    /// timing analysis, making it appropriate for cryptography.
    ///
    /// a and b must both be of the same type: either str (ASCII only),
    /// or any bytes-like object.
    ///
    /// Note: If a and b are of different lengths, or if an error occurs,
    /// a timing attack could theoretically reveal information about the
    /// types and lengths of a and b--but not their values.
    #[op]
    fn compare_digest(it: &mut Interp, a: &Value, b: &Value) -> R<bool> {
        super::compare_digest(it, a, b)
    }

    /// Determine the OpenSSL FIPS mode of operation.
    ///
    /// For OpenSSL 3.0.0 and newer it returns the state of the default provider
    /// in the default OSSL context. It's not quite the same as FIPS_mode() but good
    /// enough for unittests.
    ///
    /// Effectively any non-zero return value indicates FIPS mode;
    /// values other than 1 may have additional significance.
    #[op]
    fn get_fips_mode() -> i64 {
        0
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let value_error = it.exc_type("ValueError");
        let unsupported = crate::builtins::native::new_type(it, "_hashlib", "UnsupportedDigestmodError", Some(&value_error), Layout::Exception);
        dict_set_str(&d, "UnsupportedDigestmodError", Value::Obj(unsupported.clone()));
        it.native_state::<State>().unsupported = Some(unsupported);
        let hash = type_object::<Hash>(it);
        dict_set_str(&d, "HASHXOF", Value::Obj(hash));
        let names = Value::list(NAMES.iter().map(|(n, _)| Value::str(n)).collect());
        let fs = Value::Obj(it.types.frozenset.clone());
        if let Ok(set) = it.call(&fs, vec![names], Vec::new()) {
            dict_set_str(&d, "openssl_md_meth_names", set);
        }
        let ctors = it.new_dict();
        for (name, _) in NAMES {
            if let Some(f) = crate::vm::dict_get_str(&d, &format!("openssl_{name}")) {
                let _ = it.dict_set(&ctors, f, Value::str(name));
            }
        }
        dict_set_str(&d, "_constructors", Value::Obj(ctors));
    }
}

/// `_md5`.
#[lumen_bind::module(name = "_md5")]
pub mod _md5 {
    use super::*;

    /// Return a new MD5 hash object; optionally initialized with a string.
    #[op]
    fn md5(it: &mut Interp, #[kw] string: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Md5, string, None)
    }
}

/// `_sha1`.
#[lumen_bind::module(name = "_sha1")]
pub mod _sha1 {
    use super::*;

    /// Return a new SHA1 hash object; optionally initialized with a string.
    #[op]
    fn sha1(it: &mut Interp, #[kw] string: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha1, string, None)
    }
}

/// `_sha2`.
#[lumen_bind::module(name = "_sha2")]
pub mod _sha2 {
    use super::*;

    /// Return a new SHA-224 hash object; optionally initialized with a string.
    #[op]
    fn sha224(it: &mut Interp, #[kw] string: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha224, string, None)
    }

    /// Return a new SHA-256 hash object; optionally initialized with a string.
    #[op]
    fn sha256(it: &mut Interp, #[kw] string: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha256, string, None)
    }

    /// Return a new SHA-384 hash object; optionally initialized with a string.
    #[op]
    fn sha384(it: &mut Interp, #[kw] string: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha384, string, None)
    }

    /// Return a new SHA-512 hash object; optionally initialized with a string.
    #[op]
    fn sha512(it: &mut Interp, #[kw] string: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha512, string, None)
    }
}

/// `_sha3`.
#[lumen_bind::module(name = "_sha3")]
pub mod _sha3 {
    use super::*;

    /// Return a new SHA3 hash object.
    #[op]
    fn sha3_224(it: &mut Interp, data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha3_224, data, None)
    }

    /// Return a new SHA3 hash object.
    #[op]
    fn sha3_256(it: &mut Interp, data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha3_256, data, None)
    }

    /// Return a new SHA3 hash object.
    #[op]
    fn sha3_384(it: &mut Interp, data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha3_384, data, None)
    }

    /// Return a new SHA3 hash object.
    #[op]
    fn sha3_512(it: &mut Interp, data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Sha3_512, data, None)
    }

    /// Return a new SHAKE hash object.
    #[op]
    fn shake_128(it: &mut Interp, data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Shake128, data, None)
    }

    /// Return a new SHAKE hash object.
    #[op]
    fn shake_256(it: &mut Interp, data: Option<&Value>, #[kwonly] #[default(true)] usedforsecurity: bool) -> R<Value> {
        let _ = usedforsecurity;
        construct(it, Algo::Shake256, data, None)
    }
}

/// The parameters of a `blake2b` / `blake2s` constructor, checked as CPython's `py_blake2*_new`.
struct Blake2Args<'a> {
    data: Option<&'a Value>,
    digest_size: i64,
    key: Option<&'a Value>,
    salt: Option<&'a Value>,
    person: Option<&'a Value>,
    tree: [i64; 6],
    last_node: bool,
}

fn blake2_new(it: &mut Interp, wide: bool, a: Blake2Args) -> R<Blake2> {
    let max = Blake2::max_size(wide) as i64;
    let salt_max = Blake2::salt_size(wide);
    if !(1..=max).contains(&a.digest_size) {
        return Err(it.value_error(&format!("digest_size must be between 1 and {max} bytes")));
    }
    let bytes = |it: &mut Interp, v: Option<&Value>| -> R<Vec<u8>> {
        match v {
            Some(v) => data_of(it, v),
            None => Ok(Vec::new()),
        }
    };
    let salt = bytes(it, a.salt)?;
    if salt.len() > salt_max {
        return Err(it.value_error(&format!("maximum salt length is {salt_max} bytes")));
    }
    let person = bytes(it, a.person)?;
    if person.len() > salt_max {
        return Err(it.value_error(&format!("maximum person length is {salt_max} bytes")));
    }
    let [fanout, depth, leaf_size, node_offset, node_depth, inner_size] = a.tree;
    if !(0..=255).contains(&fanout) {
        return Err(it.value_error("fanout must be between 0 and 255"));
    }
    if !(1..=255).contains(&depth) {
        return Err(it.value_error("depth must be between 1 and 255"));
    }
    if !(0..=u32::MAX as i64).contains(&leaf_size) {
        return Err(it.new_exc_str("OverflowError", "leaf_size is too large"));
    }
    if node_offset < 0 || (!wide && node_offset > (1i64 << 48) - 1) {
        return Err(it.new_exc_str("OverflowError", "node_offset is too large"));
    }
    if !(0..=255).contains(&node_depth) {
        return Err(it.value_error("node_depth must be between 0 and 255"));
    }
    if !(0..=max).contains(&inner_size) {
        return Err(it.value_error(&format!("inner_size must be between 0 and is {max}")));
    }
    let key = bytes(it, a.key)?;
    if key.len() > max as usize {
        return Err(it.value_error(&format!("maximum key length is {max} bytes")));
    }
    if a.tree != [1, 1, 0, 0, 0, 0] || a.last_node {
        return Err(it.value_error("BLAKE2 tree hashing parameters are not supported"));
    }
    let mut b = Blake2::new(wide, a.digest_size as usize, &key, &salt, &person);
    if let Some(d) = a.data {
        let d = data_of(it, d)?;
        b.update(&d);
    }
    Ok(b)
}

/// `_blake2`.
#[lumen_bind::module(name = "_blake2")]
pub mod _blake2 {
    #![allow(clippy::too_many_arguments, clippy::new_ret_no_self)]

    use super::*;
    use crate::bind::{opaque_instance, type_object, This};
    use crate::vm::dict_set_str;

    /// Return a new BLAKE2b hash object.
    #[class(name = "blake2b", module = "_blake2", hint(py(final)))]
    pub struct Blake2b(Blake2);

    #[methods]
    impl Blake2b {
        #[constructor]
        fn new(
            cls: This<Value>,
            it: &mut Interp,
            data: Option<&Value>,
            #[kwonly] #[default(64)] digest_size: i64,
            #[kwonly] key: Option<&Value>,
            #[kwonly] salt: Option<&Value>,
            #[kwonly] person: Option<&Value>,
            #[kwonly] #[default(1)] fanout: i64,
            #[kwonly] #[default(1)] depth: i64,
            #[kwonly] #[default(0)] leaf_size: i64,
            #[kwonly] #[default(0)] node_offset: i64,
            #[kwonly] #[default(0)] node_depth: i64,
            #[kwonly] #[default(0)] inner_size: i64,
            #[kwonly] #[default(false)] last_node: bool,
            #[kwonly] #[default(true)] usedforsecurity: bool,
        ) -> R<Value> {
            let _ = usedforsecurity;
            let tree = [fanout, depth, leaf_size, node_offset, node_depth, inner_size];
            let b = blake2_new(it, true, Blake2Args { data, digest_size, key, salt, person, tree, last_node })?;
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            Ok(opaque_instance(cls, Blake2b(b)))
        }

        /// Update this hash object's state with the provided bytes-like object.
        fn update(slf: This<Py<Self>>, it: &mut Interp, data: &Value) -> R<()> {
            let data = data_of(it, data)?;
            slf.0.with(it, |s| s.0.update(&data))
        }

        /// Return the digest value as a bytes object.
        fn digest(&self) -> Vec<u8> {
            self.0.finish()
        }

        /// Return the digest value as a string of hexadecimal digits.
        fn hexdigest(&self) -> String {
            hex_encode(&self.0.finish())
        }

        /// Return a copy of the hash object.
        fn copy(&self) -> Blake2b {
            Blake2b(self.0.clone())
        }

        #[getter]
        fn name(&self) -> &'static str {
            "blake2b"
        }

        #[getter]
        fn digest_size(&self) -> usize {
            self.0.digest_size()
        }

        #[getter]
        fn block_size(&self) -> usize {
            self.0.block_size()
        }
    }

    /// Return a new BLAKE2s hash object.
    #[class(name = "blake2s", module = "_blake2", hint(py(final)))]
    pub struct Blake2s(Blake2);

    #[methods]
    impl Blake2s {
        #[constructor]
        fn new(
            cls: This<Value>,
            it: &mut Interp,
            data: Option<&Value>,
            #[kwonly] #[default(32)] digest_size: i64,
            #[kwonly] key: Option<&Value>,
            #[kwonly] salt: Option<&Value>,
            #[kwonly] person: Option<&Value>,
            #[kwonly] #[default(1)] fanout: i64,
            #[kwonly] #[default(1)] depth: i64,
            #[kwonly] #[default(0)] leaf_size: i64,
            #[kwonly] #[default(0)] node_offset: i64,
            #[kwonly] #[default(0)] node_depth: i64,
            #[kwonly] #[default(0)] inner_size: i64,
            #[kwonly] #[default(false)] last_node: bool,
            #[kwonly] #[default(true)] usedforsecurity: bool,
        ) -> R<Value> {
            let _ = usedforsecurity;
            let tree = [fanout, depth, leaf_size, node_offset, node_depth, inner_size];
            let b = blake2_new(it, false, Blake2Args { data, digest_size, key, salt, person, tree, last_node })?;
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            Ok(opaque_instance(cls, Blake2s(b)))
        }

        /// Update this hash object's state with the provided bytes-like object.
        fn update(slf: This<Py<Self>>, it: &mut Interp, data: &Value) -> R<()> {
            let data = data_of(it, data)?;
            slf.0.with(it, |s| s.0.update(&data))
        }

        /// Return the digest value as a bytes object.
        fn digest(&self) -> Vec<u8> {
            self.0.finish()
        }

        /// Return the digest value as a string of hexadecimal digits.
        fn hexdigest(&self) -> String {
            hex_encode(&self.0.finish())
        }

        /// Return a copy of the hash object.
        fn copy(&self) -> Blake2s {
            Blake2s(self.0.clone())
        }

        #[getter]
        fn name(&self) -> &'static str {
            "blake2s"
        }

        #[getter]
        fn digest_size(&self) -> usize {
            self.0.digest_size()
        }

        #[getter]
        fn block_size(&self) -> usize {
            self.0.block_size()
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        for (wide, prefix) in [(true, "BLAKE2B"), (false, "BLAKE2S")] {
            let size = Blake2::max_size(wide) as i64;
            let salt = Blake2::salt_size(wide) as i64;
            let ty = if wide { type_object::<Blake2b>(it) } else { type_object::<Blake2s>(it) };
            let attrs = [("SALT_SIZE", salt), ("PERSON_SIZE", salt), ("MAX_KEY_SIZE", size), ("MAX_DIGEST_SIZE", size)];
            for (name, v) in attrs {
                let _ = it.set_attr_str(&Value::Obj(ty.clone()), name, Value::Int(v));
                dict_set_str(&d, &format!("{prefix}_{name}"), Value::Int(v));
            }
        }
        dict_set_str(&d, "_GIL_MINSIZE", Value::Int(2048));
    }
}
