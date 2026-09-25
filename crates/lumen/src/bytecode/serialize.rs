//! (De)serialization of compiled [`Chunk`]s for ahead-of-time blobs (the bytecode section of
//! [`crate::precompiled`]).
//!
//! A serialized chunk carries exactly what [`compile`](super::compile) derives from the AST:
//! the op stream, constants, name/slot tables, parameter and `arguments` layout, the inner
//! function templates (as indices into the unit's function list — see below), the capture
//! seeding plan, and the *sizes* of the per-site caches. Runtime state is never written: inline
//! caches, name caches and their pins, object-literal templates and the optimizing tier's state
//! are rebuilt empty on decode exactly as `compile` builds them, and the activation layout is
//! recomputed from the capture plan with the same constructor `compile` uses.
//!
//! ## Function indices
//! A unit's functions are numbered in the order the stripped AST codec
//! ([`crate::snapshot`]) *enters* them: a preorder walk, a function's index assigned before
//! its params and body are written. The decoder numbers them the same way, so a chunk's inner
//! function templates (`Op::MakeClosure` targets, hoisted declarations) resolve to the very
//! `Rc<Function>` nodes of the decoded tree, as they would after a fresh compile.
//!
//! ## Section layout (one per unit; LEB128 varints unless noted)
//! ```text
//! fn_count                       functions in the unit (checked against the decoded AST)
//! string_count, strings          uv len + UTF-8 bytes each (names, slot names, string consts)
//! entry_count, entries           ascending fn index, delta-coded: uv delta, u8 kind
//!                                  kind 0 = the compiler refused this function (AST path)
//!                                  kind 1 = uv byte_len, then the chunk
//! ```
//! A function with no entry was never compiled at build time or its chunk could not be
//! serialized; it tiers up at run time as usual.
//!
//! ## Loading
//! [`attach_unit`] puts recorded refusals straight into the decoded functions' code caches and,
//! by default, only *registers* chunks (function identity → byte range; the string table is
//! not even parsed yet). [`super::compile`] consults [`take_precompiled`] first, so when a
//! function tiers up — the point a source-loaded function would compile — its chunk is decoded
//! instead (~8x cheaper than compiling it). Eager attachment (every chunk decoded at load, so
//! first calls run on the VM) is the `eager` mode, used by `LUMEN_AOT_EAGER` and verification.
//!
//! ## Chunk layout
//! ```text
//! ops            uv count, then per op: u8 tag + operands (u32/u16 as uv, bool/enums as u8)
//! consts         uv count, then per const: u8 tag (0 undefined, 1 null, 2 false, 3 true,
//!                4 number: f64 bits LE, 5 string: uv string index, 6 bigint: u8 sign + uv
//!                string index of the magnitude in hex)
//! names          uv count, uv string index each (interned like the compiler interns them)
//! slot_names     uv count, uv string index each (n_slots is their count)
//! n_params       uv
//! flags          u8: 1 uses_this, 2 env_this, 4 has arguments_slot, 8 has rest_slot
//! arguments_slot uv (only with flag 4)
//! rest_slot      uv (only with flag 8)
//! var_force_resets  uv count, uv each
//! funcs          uv count, uv unit function index each
//! cap_inits      uv count, u8 tag (0 Param(k,name) 1 Var(name) 2 Fn(k,name) 3 Lexical(name,
//!                const)) + operands
//! cache sizes    uv caches, uv obj_maps, uv name_caches (name_paths/name_pins follow
//!                name_caches; cap_caches/cap_pins follow names)
//! ```
//!
//! The op codec is generated from one table (`op_codec!` below) with an exhaustive `match`,
//! so adding an `Op` variant (or changing an operand type) fails to compile here until the
//! table is updated; the table's text is hashed into [`FINGERPRINT`], which the blob header
//! records, so a blob built against a different op set is rejected on load rather than
//! misdecoded. Operand *values* (slot/name/cache indices) are not range-checked on decode: a
//! blob is trusted like the code it is linked into (version + fingerprint checked); structural
//! corruption (truncation, bad tags, bad string/function indices) is a clean error.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::{
    activation, intern_name, ArithKind, CapInit, Chunk, CmpKind, IcState, NameIc, Op, TypeofKind,
    UpdKind,
};
use crate::ast::Function;
use crate::bigint::JsBigInt;
use crate::value::Value;

type R<T> = Result<T, String>;

/// Bump on any change to the chunk layout that the tables below do not capture.
const CODEC_VERSION: u32 = 1;

// ---- runtime counters (verification / diagnostics) --------------------------------------------

// Process-wide (a tree-walked async body can run on a coroutine thread).
static COMPILES: AtomicU64 = AtomicU64::new(0);
static ATTACHED: AtomicU64 = AtomicU64::new(0);
static REGISTERED: AtomicU64 = AtomicU64::new(0);
static REFUSALS: AtomicU64 = AtomicU64::new(0);

/// Called by [`super::compile`] on every real compile (not on a precompiled chunk).
#[inline]
pub(crate) fn note_compile() {
    COMPILES.fetch_add(1, Relaxed);
}

/// (compiles, chunks registered for on-demand decode, chunks decoded into a code cache,
/// compiler refusals attached) since the last [`reset_counters`].
pub(crate) fn counters() -> (u64, u64, u64, u64) {
    (
        COMPILES.load(Relaxed),
        REGISTERED.load(Relaxed),
        ATTACHED.load(Relaxed),
        REFUSALS.load(Relaxed),
    )
}

pub(crate) fn reset_counters() {
    for c in [&COMPILES, &REGISTERED, &ATTACHED, &REFUSALS] {
        c.store(0, Relaxed);
    }
}

// ---- primitive writer / reader ----------------------------------------------------------------

fn uv(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

struct Rd<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Rd<'a> {
    fn u8(&mut self) -> R<u8> {
        let v = *self.b.get(self.pos).ok_or("bytecode: truncated")?;
        self.pos += 1;
        Ok(v)
    }
    fn uv(&mut self) -> R<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift >= 64 {
                return Err("bytecode: varint overflow".into());
            }
        }
    }
    fn len(&mut self) -> R<usize> {
        let n = self.uv()? as usize;
        // Every counted item takes at least one byte: a count past the remaining input is
        // corruption, not a reason to allocate.
        if n > self.b.len() - self.pos {
            return Err("bytecode: bad count".into());
        }
        Ok(n)
    }
    fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or("bytecode: bad length")?;
        let s = self.b.get(self.pos..end).ok_or("bytecode: truncated")?;
        self.pos = end;
        Ok(s)
    }
}

/// One operand type of the op codec.
trait Operand: Sized {
    fn put(self, w: &mut Vec<u8>);
    fn get(r: &mut Rd) -> R<Self>;
}

impl Operand for u32 {
    fn put(self, w: &mut Vec<u8>) {
        uv(w, self as u64);
    }
    fn get(r: &mut Rd) -> R<Self> {
        u32::try_from(r.uv()?).map_err(|_| "bytecode: u32 operand out of range".into())
    }
}

impl Operand for u16 {
    fn put(self, w: &mut Vec<u8>) {
        uv(w, self as u64);
    }
    fn get(r: &mut Rd) -> R<Self> {
        u16::try_from(r.uv()?).map_err(|_| "bytecode: u16 operand out of range".into())
    }
}

impl Operand for u8 {
    fn put(self, w: &mut Vec<u8>) {
        w.push(self);
    }
    fn get(r: &mut Rd) -> R<Self> {
        r.u8()
    }
}

impl Operand for bool {
    fn put(self, w: &mut Vec<u8>) {
        w.push(self as u8);
    }
    fn get(r: &mut Rd) -> R<Self> {
        match r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            t => Err(format!("bytecode: bad bool {t}")),
        }
    }
}

/// A fieldless operand enum: exhaustive both ways, and its table text feeds the fingerprint.
macro_rules! enum_codec {
    ($sig:ident, $ty:ident { $($tag:literal => $v:ident),* $(,)? }) => {
        const $sig: &str = stringify!($ty { $($tag => $v),* });
        impl Operand for $ty {
            fn put(self, w: &mut Vec<u8>) {
                w.push(match self { $($ty::$v => $tag),* });
            }
            fn get(r: &mut Rd) -> R<Self> {
                Ok(match r.u8()? {
                    $($tag => $ty::$v,)*
                    t => return Err(format!(concat!("bytecode: bad ", stringify!($ty), " {}"), t)),
                })
            }
        }
    };
}

enum_codec!(UPD_SIG, UpdKind {
    0 => PreInc, 1 => PreDec, 2 => PostInc, 3 => PostDec, 4 => IncDiscard, 5 => DecDiscard,
});
enum_codec!(TYPEOF_SIG, TypeofKind {
    0 => Undefined, 1 => Object, 2 => Boolean, 3 => Number, 4 => BigInt, 5 => String,
    6 => Symbol, 7 => Function,
});
enum_codec!(ARITH_SIG, ArithKind {
    0 => Add, 1 => Sub, 2 => Mul, 3 => Div, 4 => Mod, 5 => BitAnd, 6 => BitOr, 7 => BitXor,
    8 => Shl, 9 => Shr, 10 => UShr,
});
enum_codec!(CMP_SIG, CmpKind {
    0 => Lt, 1 => Gt, 2 => Le, 3 => Ge, 4 => EqEq, 5 => NotEq, 6 => StrictEq, 7 => StrictNotEq,
});

/// The op table: `tag => Variant(operand: Type, …);`. Generates `enc_op`/`dec_op` (exhaustive
/// over `Op`: a new variant is a compile error here) and `OP_SIG`, the table's text.
macro_rules! op_codec {
    ($($tag:literal => $name:ident $(( $($f:ident : $t:ty),* ))? ;)*) => {
        const OP_SIG: &str = stringify!($($tag => $name $(( $($t),* ))? ;)*);
        #[cfg(test)]
        const OP_TAGS: &[u8] = &[$($tag),*];
        fn enc_op(w: &mut Vec<u8>, op: Op) {
            match op {
                $(Op::$name $(( $($f),* ))? => {
                    w.push($tag);
                    $($(<$t as Operand>::put($f, w);)*)?
                })*
            }
        }
        fn dec_op(r: &mut Rd) -> R<Op> {
            Ok(match r.u8()? {
                $($tag => Op::$name $(( $(<$t as Operand>::get(r)?),* ))?,)*
                t => return Err(format!("bytecode: bad op tag {t}")),
            })
        }
    };
}

op_codec! {
    0 => Const(a: u32);
    1 => Undef;
    2 => Dup;
    3 => Pop;
    4 => LoadLocal(a: u16);
    5 => StoreLocal(a: u16);
    6 => LoadCap(a: u32);
    7 => StoreCap(a: u32);
    8 => StoreCapInit(a: u32);
    9 => UpdateCap(a: u32, b: UpdKind);
    10 => UpdateName(a: u32, b: UpdKind);
    11 => UpdateNameCached(a: u32, b: u32, c: UpdKind);
    12 => MakeClosure(a: u32, b: u32);
    13 => UpdateLocal(a: u16, b: UpdKind);
    14 => Tdz(a: u16);
    15 => LoadName(a: u32, b: u32);
    16 => StoreName(a: u32);
    17 => StoreNameCached(a: u32, b: u32);
    18 => LoadThis;
    19 => LoadLexicalThis;
    20 => GetProp(a: u32, b: u32);
    21 => GetPropThis(a: u32, b: u32);
    22 => GetPropLocal(a: u16, b: u32, c: u32);
    23 => SetProp(a: u32, b: u32);
    24 => SetPropDrop(a: u32, b: u32);
    25 => SetPropThisDrop(a: u32, b: u32);
    26 => SetPropLocalDrop(a: u16, b: u32, c: u32);
    27 => ToStr;
    28 => GetIter;
    29 => ForInKeys;
    30 => ForInStepL(a: u16, b: u16, c: u16);
    31 => IterStepL(a: u16, b: u16);
    32 => IterCloseL(a: u16);
    33 => IterAbortL(a: u16);
    34 => DestructureGuard;
    35 => DestructureArr(a: u16);
    36 => DeleteProp(a: u32, b: bool);
    37 => DeleteElem(a: bool);
    38 => CallSpread(a: u16);
    39 => CallSpreadThis(a: u16);
    40 => AppendProp(a: u32, b: u32);
    41 => GetElem;
    42 => SetElem;
    43 => SetElemDrop;
    44 => GetElemLocal(a: u16);
    45 => SetElemLocal(a: u16);
    46 => SetElemLocalDrop(a: u16);
    47 => UpdateProp(a: u32, b: u32, c: UpdKind);
    48 => UpdateElem(a: UpdKind);
    49 => ToPropKey;
    50 => ToPropKeyLocal(a: u16);
    51 => Dup2;
    52 => GetMethod(a: u32, b: u32);
    53 => GetMethodElem;
    54 => Add;
    55 => Sub;
    56 => Mul;
    57 => Div;
    58 => Mod;
    59 => BitAnd;
    60 => BitOr;
    61 => BitXor;
    62 => Shl;
    63 => Shr;
    64 => UShr;
    65 => Lt;
    66 => Gt;
    67 => Le;
    68 => Ge;
    69 => EqEq;
    70 => NotEq;
    71 => StrictEq;
    72 => StrictNotEq;
    73 => InstanceOf(a: u32);
    74 => GenBin(a: u32);
    75 => Neg;
    76 => Plus;
    77 => Not;
    78 => BitNot;
    79 => Typeof;
    80 => TypeofIs(a: TypeofKind, b: bool);
    81 => TypeofName(a: u32);
    82 => Void;
    83 => Jump(a: u32);
    84 => JumpIfFalse(a: u32);
    85 => JumpIfNotCmp(a: CmpKind, b: u32);
    86 => JumpIfNotCmpLL(a: CmpKind, b: u16, c: u16, d: u32);
    87 => JumpIfNotCmpLK(a: CmpKind, b: u16, c: u32, d: u32);
    88 => ArithLL(a: ArithKind, b: u16, c: u16, d: u16);
    89 => ArithLK(a: ArithKind, b: u16, c: u16, d: u32);
    90 => JumpIfFalsePeek(a: u32);
    91 => JumpIfTruePeek(a: u32);
    92 => JumpIfNotNullishPeek(a: u32);
    93 => Call(a: u16);
    94 => LoadNameForCall(a: u32, b: u32);
    95 => CallWithThis(a: u16);
    96 => New(a: u16);
    97 => MakeRegExp(a: u32, b: u32);
    98 => MakeArray(a: u16);
    99 => MakeObject(a: u32, b: u16, c: u32);
    100 => Throw;
    101 => Return;
    102 => ReturnUndef;
    103 => Await;
    104 => PushHandler(a: u32);
    105 => PopHandler;
    106 => GetPrivate(a: u32);
    107 => SetPrivate(a: u32);
    108 => GetPrivateMethod(a: u32);
    109 => PrivateIn(a: u32);
    110 => UpdatePrivate(a: u32, b: UpdKind);
    111 => NewObject;
    112 => InitProp(a: u32, b: bool);
    113 => InitPropComputed(a: bool);
    114 => InitMethod(a: u32, b: u32, c: u16);
    115 => CopyDataProps;
    116 => SetProtoLit;
    117 => MakeClass(a: u32, b: u32);
    118 => Nip;
    119 => SuperGet(a: u32);
    120 => SuperGetElem;
    121 => SuperBase;
    122 => SuperMethod(a: u32, b: bool);
    123 => SuperMethodElem(a: bool);
    124 => ObjRest(a: u32, b: u16);
    125 => NewArrayLit;
    126 => ArrayAppend;
    127 => ArrayAppendSpread;
    128 => ArrayHole;
    129 => SwitchLK(a: u16, b: u32);
    130 => SuperCtor;
    131 => SuperCall(a: u16);
    132 => SuperCallSpread(a: u16);
    133 => DerivedReturn;
    134 => InitialYield;
    135 => Yield;
    136 => YieldDelegate(a: u16);
    137 => LoadCallee;
    138 => DefineField(a: u32, b: bool);
    139 => BlkNew(a: u16, b: u16);
    140 => BlkDecl(a: u16, b: u32, c: bool);
    141 => BlkCopy(a: u16);
    142 => BlkLoad(a: u16, b: u32);
    143 => BlkStore(a: u16, b: u32);
    144 => BlkInit(a: u16, b: u32);
    145 => BlkUpdate(a: u16, b: u32, c: UpdKind);
    146 => InEnv(a: u16);
    147 => TailCall(a: u16, b: bool);
    148 => ImportCall(a: u8, b: bool);
    149 => GetAsyncIter;
    150 => AsyncIterNext(a: u16, b: u16, c: u16);
    151 => AsyncIterResult(a: u16);
    152 => AsyncCloseCall(a: u16, b: u16);
    153 => AsyncCloseCheck(a: u16);
    154 => AsyncDelegateInit;
    155 => AsyncDelegateCall(a: u16, b: u16, c: u16);
    156 => AsyncDelegateResult(a: u16, b: u16);
    157 => AsyncDelegateCloseReject(a: u16, b: u16);
    158 => AsyncDelegateSpecial(a: u16, b: bool);
    159 => NewSpread(a: u16);
    160 => IterRestL(a: u16, b: u16);
    161 => TailCallSpread(a: u16, b: bool);
    162 => ArrayCbGuard(a: u8);
    163 => ArrayCbHas;
    164 => ArrayCbDone(a: u32);
    165 => ArgsLen(a: u16, b: u16);
    166 => ArgsGet(a: u16, b: u16);
    167 => LoadNewTarget;
    168 => Concat(a: u16);
}

/// The chunk fields this codec writes, in order (hand-maintained next to `enc_chunk`, whose
/// exhaustive destructuring of `Chunk` forces a look here when a field is added).
const CHUNK_SIG: &str = "ops consts(undef null false true num str bigint) names slot_names \
    n_params flags(uses_this env_this arguments_slot rest_slot derived reflect_args virt_base) var_force_resets funcs \
    cap_inits(param var fn lexical) caches obj_maps name_caches positions";

const fn fnv(mut h: u64, s: &[u8]) -> u64 {
    let mut i = 0;
    while i < s.len() {
        h = (h ^ s[i] as u64).wrapping_mul(0x0100_0000_01b3);
        i += 1;
    }
    h
}

/// The bytecode encoding's identity: codec version, the op table, the operand enum tables and
/// the chunk field list, hashed. Recorded in a blob's header (`layout_fp`); a blob whose value
/// differs from the loading lumen's is rejected. Never 0 (0 means "no bytecode").
pub(crate) const FINGERPRINT: u64 = {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    h = fnv(h, &CODEC_VERSION.to_le_bytes());
    h = fnv(h, OP_SIG.as_bytes());
    h = fnv(h, UPD_SIG.as_bytes());
    h = fnv(h, TYPEOF_SIG.as_bytes());
    h = fnv(h, ARITH_SIG.as_bytes());
    h = fnv(h, CMP_SIG.as_bytes());
    h = fnv(h, CHUNK_SIG.as_bytes());
    if h == 0 {
        1
    } else {
        h
    }
};

// ---- string table -----------------------------------------------------------------------------

#[derive(Default)]
struct Strings {
    list: Vec<Rc<str>>,
    map: HashMap<Rc<str>, u32>,
}

impl Strings {
    fn idx(&mut self, s: &str) -> u32 {
        if let Some(&i) = self.map.get(s) {
            return i;
        }
        let i = self.list.len() as u32;
        let rc: Rc<str> = Rc::from(s);
        self.list.push(rc.clone());
        self.map.insert(rc, i);
        i
    }
}

/// The decoded side: raw strings, interned on first use as a name.
struct StrTable<'a> {
    raw: Vec<&'a str>,
    interned: Vec<Option<Rc<str>>>,
}

impl StrTable<'_> {
    fn raw(&self, i: u64) -> R<&str> {
        self.raw
            .get(i as usize)
            .copied()
            .ok_or_else(|| "bytecode: bad string index".into())
    }
    /// The shared `Rc<str>` for entry `i`, interned exactly as the compiler interns names (so
    /// pointer-keyed caches see the same allocation a fresh compile would hand them).
    fn name(&mut self, i: u64) -> R<Rc<str>> {
        let slot = self
            .interned
            .get_mut(i as usize)
            .ok_or("bytecode: bad string index")?;
        if let Some(s) = slot {
            return Ok(s.clone());
        }
        let s = intern_name(self.raw[i as usize]);
        *slot = Some(s.clone());
        Ok(s)
    }
}

// ---- chunk encode -----------------------------------------------------------------------------

/// Serialize one chunk. `fn_index` maps each inner function template (by `Rc` identity) to its
/// unit function index. `Err` = the chunk holds something this codec does not carry (the
/// function then simply has no entry and compiles at run time).
fn enc_chunk(
    out: &mut Vec<u8>,
    strings: &mut Strings,
    fn_index: &HashMap<*const Function, u32>,
    chunk: &Chunk,
) -> R<()> {
    // Exhaustive: a new `Chunk` field is a compile error here until the codec decides whether
    // it is compile output (serialize it) or runtime state (rebuild it in `dec_chunk`).
    let Chunk {
        ops,
        consts,
        names,
        n_slots,
        slot_names,
        n_params,
        arguments_slot,
        var_force_resets,
        uses_this,
        funcs,
        classes,
        rest_slot,
        virt_base,
        cap_inits,
        activation_layout: _, // recomputed from cap_inits/env_this/names
        env_this,
        caches,
        jit: _, // runtime state
        obj_maps,
        name_caches,
        name_paths,
        name_pins,
        cap_caches,
        cap_pins,
        switch_tables: _, // runtime state; sized from the SwitchLK ops on decode
        derived,
        reflect_args,
        positions,
        inline_cbs: _, // runtime state; rescanned from the ops
    } = chunk;
    if !classes.is_empty() {
        return Err("class definitions are not carried by the codec".into());
    }
    if *n_slots != slot_names.len()
        || name_paths.len() != name_caches.len()
        || name_pins.borrow().len() != name_caches.len()
        || cap_caches.len() != names.len()
        || cap_pins.borrow().len() != names.len()
    {
        return Err("chunk tables disagree with the compiler's invariants".into());
    }
    uv(out, ops.len() as u64);
    for &op in ops {
        enc_op(out, op);
    }
    uv(out, consts.len() as u64);
    for c in consts {
        match c {
            Value::Undefined => out.push(0),
            Value::Null => out.push(1),
            Value::Bool(false) => out.push(2),
            Value::Bool(true) => out.push(3),
            Value::Num(n) => {
                out.push(4);
                out.extend_from_slice(&n.to_bits().to_le_bytes());
            }
            Value::Str(s) => {
                out.push(5);
                uv(out, strings.idx(s.as_str()) as u64);
            }
            Value::BigInt(b) => {
                out.push(6);
                out.push(b.is_negative() as u8);
                let mag = if b.is_negative() { b.neg() } else { b.clone() };
                uv(out, strings.idx(&mag.to_string_radix(16)) as u64);
            }
            Value::Empty | Value::Sym(_) | Value::Obj(_) => {
                return Err("constant is not a primitive literal".into())
            }
        }
    }
    uv(out, names.len() as u64);
    for n in names {
        uv(out, strings.idx(n) as u64);
    }
    uv(out, slot_names.len() as u64);
    for n in slot_names {
        uv(out, strings.idx(n) as u64);
    }
    uv(out, *n_params as u64);
    out.push(
        (*uses_this as u8)
            | (*env_this as u8) << 1
            | (arguments_slot.is_some() as u8) << 2
            | (rest_slot.is_some() as u8) << 3
            | (*derived as u8) << 4
            | (*reflect_args as u8) << 5
            | (virt_base.is_some() as u8) << 6,
    );
    if let Some(s) = arguments_slot {
        uv(out, *s as u64);
    }
    if let Some(s) = rest_slot {
        uv(out, *s as u64);
    }
    if let Some(s) = virt_base {
        uv(out, *s as u64);
    }
    uv(out, var_force_resets.len() as u64);
    for &s in var_force_resets {
        uv(out, s as u64);
    }
    uv(out, funcs.len() as u64);
    for f in funcs {
        let i = fn_index
            .get(&Rc::as_ptr(f))
            .ok_or("inner function is not a node of the unit's AST")?;
        uv(out, *i as u64);
    }
    uv(out, cap_inits.len() as u64);
    for ci in cap_inits {
        match ci {
            CapInit::Param(k, name) => {
                out.push(0);
                uv(out, *k as u64);
                uv(out, strings.idx(name) as u64);
            }
            CapInit::Var(name) => {
                out.push(1);
                uv(out, strings.idx(name) as u64);
            }
            CapInit::Fn(k, name) => {
                out.push(2);
                uv(out, *k as u64);
                uv(out, strings.idx(name) as u64);
            }
            CapInit::Lexical(name, is_const) => {
                out.push(3);
                uv(out, strings.idx(name) as u64);
                out.push(*is_const as u8);
            }
        }
    }
    uv(out, caches.len() as u64);
    uv(out, obj_maps.len() as u64);
    uv(out, name_caches.len() as u64);
    // The call-site position table, already a compact byte string (see `positions`).
    uv(out, positions.len() as u64);
    out.extend_from_slice(positions);
    Ok(())
}

// ---- chunk decode -----------------------------------------------------------------------------

fn dec_chunk(
    r: &mut Rd,
    strings: &mut StrTable,
    funcs_of_unit: &dyn Fn(usize) -> Option<Rc<Function>>,
) -> R<Chunk> {
    let n = r.len()?;
    let mut ops = Vec::with_capacity(n);
    for _ in 0..n {
        ops.push(dec_op(r)?);
    }
    let n = r.len()?;
    let mut consts = Vec::with_capacity(n);
    for _ in 0..n {
        consts.push(match r.u8()? {
            0 => Value::Undefined,
            1 => Value::Null,
            2 => Value::Bool(false),
            3 => Value::Bool(true),
            4 => Value::Num(f64::from_bits(u64::from_le_bytes(
                r.bytes(8)?.try_into().unwrap(),
            ))),
            5 => {
                let i = r.uv()?;
                Value::Str(strings.raw(i)?.into())
            }
            6 => {
                let neg = r.u8()? != 0;
                let i = r.uv()?;
                let mag =
                    JsBigInt::parse_radix(strings.raw(i)?, 16).ok_or("bytecode: bad bigint")?;
                Value::BigInt(if neg { mag.neg() } else { mag })
            }
            t => return Err(format!("bytecode: bad const tag {t}")),
        });
    }
    let n = r.len()?;
    let mut names = Vec::with_capacity(n);
    for _ in 0..n {
        let i = r.uv()?;
        names.push(strings.name(i)?);
    }
    let n = r.len()?;
    let mut slot_names = Vec::with_capacity(n);
    for _ in 0..n {
        let i = r.uv()?;
        slot_names.push(Rc::from(strings.raw(i)?));
    }
    let n_params = r.uv()? as usize;
    let flags = r.u8()?;
    let arguments_slot = if flags & 4 != 0 {
        Some(u16::get(r)?)
    } else {
        None
    };
    let rest_slot = if flags & 8 != 0 {
        Some(u16::get(r)?)
    } else {
        None
    };
    let virt_base = if flags & 64 != 0 {
        Some(u16::get(r)?)
    } else {
        None
    };
    let n = r.len()?;
    let mut var_force_resets = Vec::with_capacity(n);
    for _ in 0..n {
        var_force_resets.push(u16::get(r)?);
    }
    let n = r.len()?;
    let mut funcs = Vec::with_capacity(n);
    for _ in 0..n {
        let i = r.uv()? as usize;
        funcs.push(funcs_of_unit(i).ok_or("bytecode: bad function index")?);
    }
    let n = r.len()?;
    let mut cap_inits = Vec::with_capacity(n);
    for _ in 0..n {
        cap_inits.push(match r.u8()? {
            0 => {
                let k = u16::get(r)?;
                let i = r.uv()?;
                CapInit::Param(k, Rc::from(strings.raw(i)?))
            }
            1 => {
                let i = r.uv()?;
                CapInit::Var(Rc::from(strings.raw(i)?))
            }
            2 => {
                let k = u16::get(r)?;
                let i = r.uv()?;
                CapInit::Fn(k, Rc::from(strings.raw(i)?))
            }
            3 => {
                let i = r.uv()?;
                let name = Rc::from(strings.raw(i)?);
                CapInit::Lexical(name, bool::get(r)?)
            }
            t => return Err(format!("bytecode: bad cap init tag {t}")),
        });
    }
    let n_caches = r.uv()? as usize;
    let n_obj_maps = r.uv()? as usize;
    let n_name_caches = r.uv()? as usize;
    let n_positions = r.len()?;
    let positions: Box<[u8]> = r.bytes(n_positions)?.into();
    if n_caches > u32::MAX as usize || n_obj_maps > u32::MAX as usize {
        return Err("bytecode: bad cache count".into());
    }
    let env_this = flags & 2 != 0;
    // Runtime state below: built exactly as `compile` builds it.
    let cap_cache_len = names.len();
    let n_switch_tables = ops
        .iter()
        .filter_map(|op| match op {
            Op::SwitchLK(_, t) => Some(*t as usize + 1),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let activation_layout = activation::ActivationLayout::new(&cap_inits, env_this, &names);
    Ok(Chunk {
        ops,
        consts,
        names,
        n_slots: slot_names.len(),
        slot_names,
        n_params,
        arguments_slot,
        var_force_resets,
        uses_this: flags & 1 != 0,
        funcs,
        classes: Vec::new(),
        rest_slot,
        virt_base,
        cap_inits,
        activation_layout,
        env_this,
        caches: (0..n_caches).map(|_| Cell::new(IcState::EMPTY)).collect(),
        jit: Default::default(),
        obj_maps: (0..n_obj_maps).map(|_| OnceCell::new()).collect(),
        name_pins: RefCell::new(vec![None; n_name_caches]),
        name_paths: (0..n_name_caches).map(|_| RefCell::new(None)).collect(),
        name_caches: (0..n_name_caches)
            .map(|_| Cell::new(NameIc::EMPTY))
            .collect(),
        cap_caches: vec![Cell::new(NameIc::EMPTY); cap_cache_len],
        cap_pins: RefCell::new(vec![None; cap_cache_len]),
        switch_tables: (0..n_switch_tables).map(|_| OnceCell::new()).collect(),
        derived: flags & 16 != 0,
        reflect_args: flags & 32 != 0,
        positions,
        inline_cbs: Default::default(),
    })
}

// ---- unit sections ----------------------------------------------------------------------------

/// What building a unit's bytecode section did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SectionStats {
    /// Functions in the unit.
    pub functions: usize,
    /// Functions whose chunk is in the section.
    pub chunks: usize,
    /// Functions the bytecode compiler refused (recorded, so run time does not retry).
    pub refused: usize,
    /// Functions that compiled but whose chunk this codec could not carry (compile at run time).
    pub unserializable: usize,
}

/// Compile every function of a unit (`funcs`: the unit's functions in function-index order,
/// as the stripped AST encoder returns them) and serialize the results.
/// The directive that marks a function as run-once (see [`encode_unit`]).
pub const RUN_ONCE: &str = "lumen:run-once";

/// Whether `f`'s body opens with the [`RUN_ONCE`] directive.
fn is_run_once(f: &Function) -> bool {
    f.parsed_body().is_some_and(|body| {
        matches!(body.first(), Some(crate::ast::Stmt::Expr(crate::ast::Expr::Str(s))) if &**s == RUN_ONCE)
    })
}

pub(crate) fn encode_unit(funcs: &[Rc<Function>]) -> (Vec<u8>, SectionStats) {
    let fn_index: HashMap<*const Function, u32> = funcs
        .iter()
        .enumerate()
        .map(|(i, f)| (Rc::as_ptr(f), i as u32))
        .collect();
    let mut strings = Strings::default();
    let mut stats = SectionStats {
        functions: funcs.len(),
        ..SectionStats::default()
    };
    // (index, None = refused | Some(chunk bytes))
    let mut entries: Vec<(u32, Option<Vec<u8>>)> = Vec::new();
    for (i, f) in funcs.iter().enumerate() {
        // A function whose body opens with the `"lumen:run-once"` directive (a lazily loaded
        // glue file's body, a module factory) is left to the tree-walker: its chunk would stay
        // attached to the function node for good, while a body the tree-walker ran is released
        // again by the collector.
        let run_once = is_run_once(f);
        match if run_once { None } else { super::compile(f) } {
            None => {
                stats.refused += 1;
                entries.push((i as u32, None));
            }
            Some(chunk) => {
                let mut buf = Vec::new();
                match enc_chunk(&mut buf, &mut strings, &fn_index, &chunk) {
                    Ok(()) => {
                        stats.chunks += 1;
                        entries.push((i as u32, Some(buf)));
                    }
                    Err(_) => stats.unserializable += 1,
                }
            }
        }
    }
    let mut out = Vec::new();
    uv(&mut out, funcs.len() as u64);
    uv(&mut out, strings.list.len() as u64);
    for s in &strings.list {
        uv(&mut out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }
    uv(&mut out, entries.len() as u64);
    let mut prev = 0u32;
    for (i, e) in &entries {
        uv(&mut out, (*i - prev) as u64);
        prev = *i;
        match e {
            None => out.push(0),
            Some(bytes) => {
                out.push(1);
                uv(&mut out, bytes.len() as u64);
                out.extend_from_slice(bytes);
            }
        }
    }
    (out, stats)
}

/// A parsed bytecode section: its string table, and each entry's function index and kind
/// (`None` = refusal, `Some(range)` = the chunk's bytes within the section).
struct Section {
    /// The string table's bytes (count + strings), parsed by [`parse_strings`] when needed.
    strings: std::ops::Range<usize>,
    entries: Vec<(usize, Option<std::ops::Range<usize>>)>,
}

fn parse_strings(table: &[u8]) -> R<StrTable<'_>> {
    let mut r = Rd { b: table, pos: 0 };
    let n = r.len()?;
    let mut raw = Vec::with_capacity(n);
    for _ in 0..n {
        let len = r.len()?;
        let bytes = r.bytes(len)?;
        raw.push(std::str::from_utf8(bytes).map_err(|_| "bytecode: bad utf8")?);
    }
    Ok(StrTable {
        interned: vec![None; raw.len()],
        raw,
    })
}

fn parse_section(section: &[u8], fn_count: usize) -> R<Section> {
    let mut r = Rd { b: section, pos: 0 };
    if r.uv()? as usize != fn_count {
        return Err("bytecode: function count does not match the unit's AST".into());
    }
    let strings_start = r.pos;
    let n = r.len()?;
    for _ in 0..n {
        let len = r.len()?;
        r.bytes(len)?;
    }
    let strings = strings_start..r.pos;
    let n = r.len()?;
    let mut entries = Vec::with_capacity(n);
    let mut idx = 0usize;
    for k in 0..n {
        let delta = r.uv()? as usize;
        idx = if k == 0 {
            delta
        } else {
            idx.checked_add(delta)
                .filter(|_| delta > 0)
                .ok_or("bytecode: bad entry order")?
        };
        if idx >= fn_count {
            return Err("bytecode: bad function index".into());
        }
        match r.u8()? {
            0 => entries.push((idx, None)),
            1 => {
                let len = r.len()?;
                let start = r.pos;
                r.bytes(len)?;
                entries.push((idx, Some(start..start + len)));
            }
            t => return Err(format!("bytecode: bad entry kind {t}")),
        }
    }
    if r.pos != section.len() {
        return Err("bytecode: trailing bytes in section".into());
    }
    Ok(Section { strings, entries })
}

fn dec_chunk_at(
    bytes: &[u8],
    strings: &mut StrTable,
    funcs: &dyn Fn(usize) -> Option<Rc<Function>>,
) -> R<Chunk> {
    let mut cr = Rd { b: bytes, pos: 0 };
    let chunk = dec_chunk(&mut cr, strings, funcs)?;
    if cr.pos != bytes.len() {
        return Err("bytecode: trailing bytes in chunk".into());
    }
    Ok(chunk)
}

/// A unit whose chunks decode on demand (see [`attach_unit`]).
struct LazyUnit {
    section: &'static [u8],
    /// The string table: its byte range, parsed on the first decode.
    strings_at: std::ops::Range<usize>,
    strings: RefCell<Option<StrTable<'static>>>,
    /// The unit's AST, which resolves a chunk's inner functions by index (decoding their
    /// headers if need be). Weak: the unit lives as long as any of its functions does, and a
    /// chunk is only decoded for a live function.
    ast: Weak<crate::snapshot::SplitUnit>,
    /// Each function's entry, by function index: [`NO_ENTRY`], [`REFUSED`], or the chunk's
    /// `(start, len)` within `section`.
    entries: Box<[(u32, u32)]>,
}

const NO_ENTRY: (u32, u32) = (0, 0);
const REFUSED: (u32, u32) = (u32::MAX, 0);

struct LazyEntry {
    unit: Rc<LazyUnit>,
    idx: usize,
    bytes: std::ops::Range<usize>,
}

thread_local! {
    /// Precompiled chunks not yet decoded, by function identity. Keys are only compared with a
    /// live function's address: the unit's `Weak` keeps each registered allocation from being
    /// reused, so an address match is that very function.
    static LAZY: RefCell<LazyMap> = RefCell::new(LazyMap::default());
    /// On a coroutine worker thread, the table of the thread driving it (see [`lazy_enter`]);
    /// null elsewhere.
    static LAZY_DRIVER: std::cell::Cell<*const RefCell<LazyMap>> =
        const { std::cell::Cell::new(std::ptr::null()) };
}

/// Run `f` on this thread's table of registered chunks: its own, or on a coroutine worker the
/// driver's (a body running there decodes function headers, which register their chunks).
fn with_lazy<T>(f: impl FnOnce(&RefCell<LazyMap>) -> T) -> T {
    let driver = LAZY_DRIVER.with(|d| d.get());
    if driver.is_null() {
        LAZY.with(f)
    } else {
        // SAFETY: set by `lazy_enter` to the driving thread's table, which outlives the
        // coroutine, and only this thread runs while the driver is parked (the coroutine
        // handoff is strict ping-pong).
        f(unsafe { &*driver })
    }
}

/// The table this thread's code uses, for a coroutine body it hands to a worker thread
/// (opaque; see [`lazy_enter`]).
pub(crate) fn lazy_handle() -> *const () {
    let driver = LAZY_DRIVER.with(|d| d.get());
    if driver.is_null() {
        with_lazy(|l| l as *const RefCell<LazyMap> as *const ())
    } else {
        driver as *const ()
    }
}

/// Make this (coroutine worker) thread use `table` ([`lazy_handle`] of its driver), or its own
/// again for null; returns the previous setting.
pub(crate) fn lazy_enter(table: *const ()) -> *const () {
    LAZY_DRIVER.with(|d| d.replace(table as *const RefCell<LazyMap>)) as *const ()
}

type LazyMap = HashMap<*const Function, LazyEntry, std::hash::BuildHasherDefault<PtrHasher>>;

/// Hashes a pointer key with one multiply (registration inserts one key per function of a
/// bundle; SipHash would be most of the load-time cost of the table).
#[derive(Default)]
struct PtrHasher(u64);

impl std::hash::Hasher for PtrHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        }
    }
    fn write_usize(&mut self, v: usize) {
        self.0 = ((v as u64) >> 3).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    }
}

/// Attach a unit's bytecode section to its AST: every function the unit decodes (now or
/// later — function headers decode on demand, see [`crate::snapshot::SplitUnit`]) is passed
/// here. A recorded refusal goes straight into the function's code cache (it stays on the
/// tree-walker, no compile attempt); a chunk is registered and decoded when the function first
/// tiers up (`compile` consults [`take_precompiled`]) — decoding every chunk of a large bundle
/// costs far more than the few compiles a typical load performs. A function whose cache is
/// already filled is left alone.
pub(crate) fn attach_unit(
    section: &'static [u8],
    ast: &Rc<crate::snapshot::SplitUnit>,
) -> R<()> {
    let _mem = crate::memstats::enter(crate::memstats::Cat::ChunkDecode);
    let Section {
        strings: strings_at,
        entries: list,
    } = parse_section(section, ast.fn_count())?;
    let mut entries = vec![NO_ENTRY; ast.fn_count()].into_boxed_slice();
    for (idx, e) in list {
        entries[idx] = match e {
            None => REFUSED,
            Some(range) => (range.start as u32, range.len() as u32),
        };
    }
    let unit = Rc::new(LazyUnit {
        section,
        strings_at,
        strings: RefCell::new(None),
        ast: Rc::downgrade(ast),
        entries,
    });
    with_lazy(|lazy| {
        // Drop registrations whose function died (their allocations are only pinned by the
        // unit's table), so reloading a program does not grow the table forever.
        let mut lazy = lazy.borrow_mut();
        if lazy.len() > 4096 {
            lazy.retain(|&f, e| e.unit.ast.upgrade().is_some_and(|u| u.is(e.idx, f)));
        }
    });
    ast.set_hook(Box::new(move |idx, f| register(&unit, idx, f)));
    Ok(())
}

/// [`attach_unit`]'s per-function step.
fn register(unit: &Rc<LazyUnit>, idx: usize, f: &Rc<Function>) {
    match unit.entries.get(idx).copied().unwrap_or(NO_ENTRY) {
        NO_ENTRY => {}
        REFUSED => {
            if f.code.set(None).is_ok() {
                REFUSALS.fetch_add(1, Relaxed);
            }
        }
        (start, len) => {
            if f.code.get().is_some() {
                return;
            }
            // Tier up on the first call: the chunk is ready, and running the function on the
            // tree-walker first would decode its (deferred) body.
            f.calls.set(f.calls.get().max(u32::MAX - 1));
            let entry = LazyEntry {
                unit: unit.clone(),
                idx,
                bytes: start as usize..(start + len) as usize,
            };
            with_lazy(|lazy| lazy.borrow_mut().insert(Rc::as_ptr(f), entry));
            REGISTERED.fetch_add(1, Relaxed);
        }
    }
}

/// Decode `f`'s registered chunk into its code cache now (`LUMEN_AOT_EAGER`, verification).
pub(crate) fn attach_now(f: &Rc<Function>) {
    if f.code.get().is_none() {
        if let Some(chunk) = take_precompiled(f) {
            let _ = f.code.set(Some(chunk));
        }
    }
}

/// Precompiled chunks registered and not yet decoded (for the `LUMEN_MEM_STATS` report).
pub(crate) fn lazy_registered() -> usize {
    with_lazy(|l| l.borrow().len())
}

/// The precompiled chunk registered for `func`, decoded (consumed: the caller caches it in
/// `func.code`). `None` = nothing registered, or its bytes failed to decode — the caller
/// compiles as usual.
pub(crate) fn take_precompiled(func: &Function) -> Option<Rc<Chunk>> {
    let _mem = crate::memstats::enter(crate::memstats::Cat::ChunkDecode);
    let entry = with_lazy(|lazy| {
        let mut lazy = lazy.borrow_mut();
        if lazy.is_empty() {
            return None;
        }
        lazy.remove(&(func as *const Function))
    })?;
    let unit = &entry.unit;
    let ast = unit.ast.upgrade()?;
    if !ast.is(entry.idx, func as *const Function) {
        return None;
    }
    let lookup = |i: usize| ast.function(i).ok();
    let mut strings = unit.strings.borrow_mut();
    if strings.is_none() {
        *strings = Some(parse_strings(&unit.section[unit.strings_at.clone()]).ok()?);
    }
    let chunk = dec_chunk_at(
        &unit.section[entry.bytes],
        strings.as_mut().unwrap(),
        &lookup,
    )
    .ok()?;
    ATTACHED.fetch_add(1, Relaxed);
    Some(Rc::new(chunk))
}

/// Verification: for every function of a decoded unit, compile it afresh and compare with what
/// its code cache holds (attached from a blob) — byte for byte under this codec, with the op
/// listings in the error. Returns (chunks compared, refusals confirmed).
pub(crate) fn verify_unit(funcs: &[Rc<Function>]) -> R<(usize, usize)> {
    let fn_index: HashMap<*const Function, u32> = funcs
        .iter()
        .enumerate()
        .map(|(i, f)| (Rc::as_ptr(f), i as u32))
        .collect();
    let mut strings = Strings::default();
    let (mut compared, mut refusals) = (0, 0);
    for (i, f) in funcs.iter().enumerate() {
        let Some(attached) = f.code.get() else {
            continue;
        };
        let fresh = super::compile(f);
        match (attached, fresh) {
            (None, None) => refusals += 1,
            (Some(a), Some(b)) => {
                let (mut ea, mut eb) = (Vec::new(), Vec::new());
                enc_chunk(&mut ea, &mut strings, &fn_index, a)?;
                enc_chunk(&mut eb, &mut strings, &fn_index, &b)?;
                if ea != eb {
                    return Err(format!(
                        "function #{i} ({:?}): attached chunk differs from a fresh compile\n\
                         attached: {:?}\nfresh:    {:?}",
                        f.name, a.ops, b.ops
                    ));
                }
                compared += 1;
            }
            (a, b) => {
                return Err(format!(
                    "function #{i} ({:?}): attached {} but a fresh compile {}",
                    f.name,
                    if a.is_some() { "a chunk" } else { "a refusal" },
                    if b.is_some() { "succeeds" } else { "refuses" },
                ))
            }
        }
    }
    Ok((compared, refusals))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_tags_are_distinct_and_dense() {
        let mut seen = [false; 256];
        for (i, &t) in OP_TAGS.iter().enumerate() {
            assert!(!seen[t as usize], "op tag {t} used twice");
            seen[t as usize] = true;
            assert_eq!(t as usize, i, "op tags should be dense and in table order");
        }
    }

    #[test]
    fn fingerprint_is_nonzero_and_covers_the_op_table() {
        assert_ne!(FINGERPRINT, 0);
        assert!(OP_SIG.contains("MakeClosure"));
    }

    /// `src`'s functions (in index order) and its split AST, as an ahead-of-time blob holds
    /// them; the unit decodes a second, independent set of functions.
    fn unit_of(src: &str) -> (Vec<Rc<Function>>, Rc<crate::snapshot::SplitUnit>) {
        let body =
            crate::parser::with_eager_bodies(|| crate::parser::parse_script(src, false)).unwrap();
        let unit = crate::snapshot::encode_split(&body, None);
        let ast: &'static [u8] = Box::leak(unit.ast.into_boxed_slice());
        let bodies: &'static [u8] = Box::leak(unit.bodies.into_boxed_slice());
        let split = crate::snapshot::SplitUnit::new(
            ast,
            Rc::from(""),
            None,
            Box::new(move |start, len| Ok(std::borrow::Cow::Borrowed(&bodies[start..start + len]))),
        )
        .unwrap();
        (unit.funcs, split)
    }

    /// Attach `section` to `unit` and decode all its functions: `(functions, chunks
    /// registered, refusals attached)`.
    fn attach_all(section: &'static [u8], unit: &Rc<crate::snapshot::SplitUnit>) -> (Vec<Rc<Function>>, usize, usize) {
        attach_unit(section, unit).unwrap();
        let funcs = unit.all_functions().unwrap();
        let chunks = funcs
            .iter()
            .filter(|f| with_lazy(|l| l.borrow().contains_key(&Rc::as_ptr(f))))
            .count();
        let refusals = funcs.iter().filter(|f| matches!(f.code.get(), Some(None))).count();
        (funcs, chunks, refusals)
    }

    #[test]
    fn chunks_roundtrip_byte_for_byte() {
        let src = r#"
            function add(a, b) { return a + b; }
            function loop(n) { let s = 0; for (let i = 0; i < n; i++) s += i * 2.5; return s; }
            function consts() { return [1n, -0, NaN, "str\u{1F600}", null, true, false, 1e300]; }
            function closures(x) { const inc = () => x++; function h() { return x; } return inc() + h(); }
            function objs(o) { const p = { a: 1, b: o.c, d: /re+/gi }; delete p.a; return typeof o === "object" && o instanceof Object; }
            function args() { return arguments.length; }
            async function aw(p) { try { return await p; } catch (e) { throw e; } }
            function* gen() { yield 1; }
            async function* agen() { yield 1; }
            function refused(o) { with (o) { return x; } }
            function sw(x) { switch (x) { case 1: return 'a'; default: return 'b'; } }
        "#;
        let (funcs, unit) = unit_of(src);
        let (section, stats) = encode_unit(&funcs);
        assert!(stats.chunks >= 7, "{stats:?}");
        assert!(stats.refused >= 1, "the `with` body is refused: {stats:?}");
        // Decode against a second, independent parse of the same program.
        let section: &'static [u8] = Box::leak(section.into_boxed_slice());
        let (funcs2, chunks, refusals) = attach_all(section, &unit);
        assert_eq!((chunks, refusals), (stats.chunks, stats.refused));
        funcs2.iter().for_each(attach_now);
        let (compared, confirmed) = verify_unit(&funcs2).unwrap();
        assert_eq!((compared, confirmed), (chunks, refusals));
    }

    #[test]
    fn lazily_registered_chunks_decode_on_first_compile() {
        let src = "function f(a) { let s = 0; for (const x of a) s += x; return s; } function g() { return () => f([1, 2]); } function* h() {}";
        let (funcs, unit) = unit_of(src);
        let (section, stats) = encode_unit(&funcs);
        let section: &'static [u8] = Box::leak(section.into_boxed_slice());
        let (funcs2, chunks, refusals) = attach_all(section, &unit);
        assert_eq!((chunks, refusals), (stats.chunks, stats.refused));
        let fn_index: HashMap<*const Function, u32> = funcs2
            .iter()
            .enumerate()
            .map(|(i, f)| (Rc::as_ptr(f), i as u32))
            .collect();
        let mut strings = Strings::default();
        let mut decoded = 0;
        for f in &funcs2 {
            if f.code.get().is_some() {
                assert!(f.is_generator, "only the refusal is attached up front");
                continue;
            }
            // The registered chunk (consumed), then a real compile: identical.
            let pre = super::super::compile(f).expect("a registered chunk");
            let fresh = super::super::compile(f).expect("compiles");
            assert!(!Rc::ptr_eq(&pre, &fresh));
            let (mut a, mut b) = (Vec::new(), Vec::new());
            enc_chunk(&mut a, &mut strings, &fn_index, &pre).unwrap();
            enc_chunk(&mut b, &mut strings, &fn_index, &fresh).unwrap();
            assert_eq!(a, b);
            decoded += 1;
        }
        assert_eq!(decoded, chunks);
    }

    #[test]
    fn run_once_functions_are_left_to_the_tree_walker() {
        let src = r#"var a = function () { "lumen:run-once"; for (;;) break; }; var b = function () { for (;;) break; };"#;
        let (funcs, unit) = unit_of(src);
        let (section, stats) = encode_unit(&funcs);
        assert_eq!((stats.chunks, stats.refused), (1, 1), "{stats:?}");
        let section: &'static [u8] = Box::leak(section.into_boxed_slice());
        let (funcs2, chunks, refusals) = attach_all(section, &unit);
        assert_eq!((chunks, refusals), (1, 1));
        assert!(matches!(funcs2[0].code.get(), Some(None)));
    }

    #[test]
    fn corrupt_sections_are_errors_not_panics() {
        let (funcs, unit) =
            unit_of("function f(a) { return a * 2 + 1; } function g() { return f(3); }");
        let (section, _) = encode_unit(&funcs);
        let section: &'static [u8] = Box::leak(section.into_boxed_slice());
        for cut in 0..section.len() {
            assert!(attach_unit(&section[..cut], &unit).is_err(), "cut at {cut}");
        }
        let (_, other) = unit_of("function f() {}");
        assert!(
            attach_unit(section, &other).is_err(),
            "function count mismatch"
        );
    }
}
