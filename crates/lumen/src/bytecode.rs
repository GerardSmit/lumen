//! Bytecode tier v0: a per-function stack VM behind the tree-walking interpreter.
//!
//! The tree-walker is the reference oracle — it passes 100% of test262 and its semantics are
//! never altered by this tier. A function is either compiled *whole* (its body contains only
//! constructs this compiler fully understands) or it runs in the tree-walker; there is no partial
//! compilation and no deoptimization. Every operation with observable semantics (property access,
//! calls, coercions, name resolution outside the function) delegates to the interpreter's own
//! helpers, so behavior differences can only come from the local-variable and dispatch layers.
//!
//! Locals live in a flat slot vector: v0 refuses any function where a local could be observed
//! from outside (inner functions/closures, direct `eval`, `with`, `arguments`, destructuring),
//! which is what makes slot storage sound. TDZ is represented by `Value::Empty` in the slot —
//! reads check it and throw the same ReferenceError the tree-walker would.
//!
//! Tier selection (see `Interp::tier`): `bytecode` (default) or `interp` (this module is never
//! entered — the tree-walker runs). Compilation kicks in at `tier_threshold` calls (0 =
//! immediately). Selectable via the `LUMEN_TIER` / `LUMEN_TIER_THRESHOLD` env vars, the CLI's
//! `--tier`, or `Engine::set_tier`.

mod activation;
pub(crate) mod array_destructure;
pub(crate) mod array_iterator_step;
pub(crate) mod class_fields;
pub(crate) mod ctor_plan;
pub(crate) mod derived;
pub(crate) mod iter_fast;
mod seed;
#[cfg(feature = "compiler")]
mod self_tail;
mod slots;
pub(crate) use slots::op_slots;
pub(crate) mod block_env;
#[cfg(feature = "compiler")]
mod destructure_assign;
mod destructure_seq;
pub(crate) mod ext_ops;
#[cfg(feature = "compiler")]
mod finally;
pub(crate) mod for_await;
pub(crate) mod for_in;
pub(crate) mod generator;
pub(crate) mod inline_callback;
#[cfg(feature = "jit")]
pub(crate) mod jit;
#[cfg(not(feature = "jit"))]
#[path = "bytecode/jit_stub.rs"]
pub(crate) mod jit;
mod memsize;
mod name_path;
pub(crate) mod reflect;
pub(crate) use memsize::ChunkBytes;
#[cfg(feature = "compiler")]
mod object_literal;
#[cfg(feature = "compiler")]
mod parameters;
pub(crate) mod positions;
mod prepared_call;
pub(crate) use prepared_call::{call_once_direct, PreparedCall};
pub(crate) mod serialize;
#[cfg(feature = "compiler")]
mod switch;
mod switch_table;
#[cfg(feature = "compiler")]
mod this_binding;
mod verify;
mod vm_regs;
#[cfg(test)]
mod write_strictness;

use crate::value::Gc;
use std::rc::Rc;

use crate::ast::*;
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;
use lumen_common::limits::size;
use vm_regs::{PcReg, VmStack};

#[cfg(feature = "jit")]
pub use jit::aot::{BuildMode, Built as NativeFunction, OP_NAMES as NATIVE_OP_NAMES};

#[cfg(feature = "jit")]
pub enum NativeCaptureInit {
    Param(u16, Rc<str>),
    Var(Rc<str>),
    Function(u16, Rc<str>),
    Lexical(Rc<str>, bool),
}

#[cfg(feature = "jit")]
pub struct NativeFrameLayout<'a> {
    pub arguments_slot: Option<u16>,
    pub rest_slot: Option<u16>,
    pub virt_base: Option<u16>,
    pub var_force_resets: &'a [u16],
    pub cap_inits: Vec<NativeCaptureInit>,
    pub uses_this: bool,
    pub env_this: bool,
    pub derived: bool,
    pub reflect_args: bool,
    pub property_cache_count: usize,
    pub name_cache_count: usize,
    pub object_template_count: usize,
}

/// Execution tier. `Interp` must not touch any codegen path at all; `Bytecode` (the default)
/// compiles eligible functions to this module's stack VM. The string `"jit"` (CLI `--tier`,
/// `LUMEN_TIER`) is still accepted as an alias for `Bytecode`: native tier being rewritten; see
/// docs/jit.md.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Interp,
    Bytecode,
}

/// Per-site property inline-cache state. `depth == IC_EMPTY` means the site has not cached yet.
/// Otherwise the property was last found as an own, non-accessor data property of the object
/// `depth` prototype hops above the receiver, at `entries` slot `slot` — and every hop below the
/// holder had *no* own property of that name. A hit re-validates all of that (each hop plain and
/// missing `name`, the holder's cached slot still keyed `name`), so a stale cache — including a
/// *different* object reaching this shared per-site cache — can only cost time, never correctness.
///
/// `recv_shape` / `holder_shape` are the receiver's and holder's [object shapes] at cache time.
/// For a `depth == 0` or `depth == 1` hit on non-exotic objects they turn validation into shape-
/// id compares (no per-hop key/hash checks): shapes are shared across structurally-identical
/// objects, so matching one recorded from object A on object B guarantees B's `slot` maps `name`
/// too. Deeper hits, exotics (arrays), and shape misses fall back to the key-checked walk.
///
/// [object shapes]: crate::value::Props::shape
#[derive(Clone, Copy)]
pub struct IcState {
    pub recv_shape: u32,
    pub holder_shape: u32,
    pub slot: u32,
    pub depth: u8,
    /// Bit 0: `mid_shape` was recorded (a `depth ≥ 2` fill whose depth-1 hop was a plain
    /// ordinary object); bit 1: `mid2_shape` too (depth 3). Flags are needed because shape id 0
    /// is a real shape (the empty object).
    pub mid_ok: u8,
    /// The intermediate (depth-1) hop's shape for a `depth == 2` hit: a match proves that hop
    /// still lacks the name, making the two-hop shape fast path sound.
    pub mid_shape: u32,
    /// The depth-2 hop's shape for a `depth == 3` hit (three-level class hierarchies put base
    /// methods three hops from an instance; without this they'd re-walk every access). Recorded
    /// iff `mid_ok & 2`.
    pub mid2_shape: u32,
}

pub const IC_EMPTY: u8 = u8::MAX;
/// `IcState::depth` marker for a cached ABSENT property: on a receiver of `recv_shape`, `name`
/// is missing along the entire (all `Exotic::None`, all ic-plain) prototype chain. The chain's
/// shapes sit in recv_shape/mid_shape/mid2_shape/holder_shape in walk order with the level
/// count in `slot` (1-4); a hit re-walks the live chain validating each shape and yields
/// `undefined`. Shape-only proof is sound for absence: a shape pins the exact key set of a
/// non-elem-mode map, and every level's exotic/side-table gates are re-checked live.
pub const IC_ABSENT: u8 = 0xFC;
/// `mid_ok` flag of an `IC_ABSENT` state: some level is an Array or a String wrapper (the
/// site's name is neither `length` nor digit-led, so their entries alone decide it).
pub const IC_ABSENT_EXOTIC: u8 = 4;
/// Way count of a property IC site: `Compiler::new_cache` allocates this many consecutive
/// cells and the probes walk all of them (a 3-4 shape site — one dispatch loop over a class
/// hierarchy — otherwise thrashes 2 ways and re-derives forever).
pub const PROP_IC_WAYS: usize = 4;
/// Flag bit OR'd into `IcState::depth` when the HOLDER is an `Exotic::Array` (including a
/// depth-0 array receiver — `arr.length`, and `Array.prototype`, itself an Array exotic, as a
/// method holder). The named prefix of an array's entries is pinned by its shape like any other
/// object's (elements live past it), so such a hit validates by shape alone; the flag only tells
/// the probes which exotic gate the holder must pass. Only meaningful while `depth < 0x80`
/// (`IC_CREATE`/`IC_EMPTY` have the bit set but are filtered by range first).
pub const IC_ARR_KEYCHK: u8 = 0x40;
/// Flag bit OR'd into `IcState::depth` when the resolved entry is an accessor with a getter:
/// the read calls it (`Interp::get_prop_ic`). The data probes, and every JIT reader of the
/// states (which want exact depths), ignore such states. Only meaningful while `depth < 0x80`.
pub const IC_GETTER: u8 = 0x20;
/// Deepest prototype hop the IC will record; hotter sites deeper than this stay on the slow path.
pub const IC_MAX_DEPTH: u8 = 4;
/// `IcState::depth` marker for a property-*creation* cache (constructor `this.x = v` on a fresh
/// shape): `recv_shape` is the shape BEFORE the insert and `mid_shape` holds the
/// [`crate::value::proto_epoch`] at fill. A hit requires the same shape, the same receiver
/// prototype *identity* (weak-pinned by identity in `Interp::creation_pins` — same shape does NOT
/// imply same proto), an unchanged epoch (no marked prototype mutated, no proto swap, no defineProperty
/// anywhere), a live `extensible` receiver, and a non-index name (guaranteed at fill): together
/// they re-prove the fill-time chain walk ("no hop has an own copy / setter / non-writable shadow
/// of this name"), so the insert can skip the whole `OrdinarySet` walk.
pub const IC_CREATE: u8 = 0xFD;

/// Per-site free-name inline-cache state (`LoadName` / `LoadNameForCall`): the last successful
/// *depth-0* resolution — the name was found directly in the scope the chunk runs under (`env`),
/// as a plain initialized binding (no `with` object on that scope, no live module import).
///
/// A hit revalidates: (1) the current env is *the same allocation* — `env` compares raw pointers,
/// which is ABA-safe because `Chunk::name_pins` holds a `Weak` to the cached scope, pinning its
/// allocation for the cache's lifetime; (2) the scope's [`crate::interpreter::VarMap`] generation
/// is unchanged — every structural map mutation bumps it, so `binding` still points at the live
/// entry *and* no insert/remove could have changed what the name resolves to. Depth-0-only is
/// what makes the generation check complete: with no intermediate scopes between start and
/// holder, there is nothing else whose mutation could re-route the name (a sloppy direct `eval`
/// hoisting into this scope, or a `delete`, is an insert/remove here and bumps the generation).
///
/// In-place binding writes don't bump the generation, so a hit reads the *live* value and the
/// live `initialized` flag through the pointer — both exactly what the slow path would see.
///
/// A second mode covers *globals* (`Math`, a script-level `var`, a top-level function): when the
/// chunk's env IS the global scope (no intermediate scopes to guard), a resolution that missed
/// the scope and landed on an own data property of the ordinary global object caches
/// `(env|1, shape<<32|slot, gen)` — the low bit of `env` tags the mode (scope pointers are
/// ≥8-aligned). A hit revalidates the scope generation (no shadowing binding appeared) and the
/// global object's shape (same ordered key layout ⇒ the slot still maps this name), then
/// re-checks `accessor` at the slot (attributes are not part of the shape).
#[derive(Clone, Copy)]
pub struct NameIc {
    /// `Rc::as_ptr` of the scope at cache time (0 = empty). Low tag bits (scope allocations
    /// are ≥8-aligned): bit 0 = global-object mode; bit 1 = depth-1 mode, where the pointer
    /// is the current env's PARENT — the current env is this chunk's activation, whose fresh
    /// per-call pointer could never hit an exact compare (see `Chunk::name_ic_fill`).
    pub env: usize,
    /// Scope mode: the resolved `&Binding` within that scope's map. Global mode: shape<<32|slot.
    /// `u64` (not `usize`) so the packing is well-defined on 32-bit targets (wasm).
    pub binding: u64,
    /// Generation of the map holding `binding` at fill time (structural changes invalidate).
    pub gen: u32,
    /// Depth-1 mode: the activation's post-construction generation — chunk-determined (the
    /// cap_inits insert count), so it validates EVERY fresh activation of this chunk while
    /// catching a sloppy inner eval's var hoisted into a live one. 0 otherwise.
    pub act_gen: u32,
}

impl NameIc {
    pub const EMPTY: NameIc = NameIc {
        env: 0,
        binding: 0,
        gen: 0,
        act_gen: 0,
    };
}

impl IcState {
    pub const EMPTY: IcState = IcState {
        recv_shape: 0,
        holder_shape: 0,
        slot: 0,
        depth: IC_EMPTY,
        mid_ok: 0,
        mid_shape: 0,
        mid2_shape: 0,
    };
}

/// Which update `UpdateLocal` performs, and the value it leaves on the stack: `Pre*` push the
/// updated value, `Post*` push the original (coerced) value, `*Discard` push nothing (the update
/// is a statement or a `for` update — its value is unobservable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdKind {
    PreInc,
    PreDec,
    PostInc,
    PostDec,
    IncDiscard,
    DecDiscard,
}

/// The result a fused `typeof v === "<literal>"` test compares against (see `Op::TypeofIs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeofKind {
    Undefined,
    Object,
    Boolean,
    Number,
    BigInt,
    String,
    Symbol,
    Function,
}

impl TypeofKind {
    /// The kind a `typeof` result literal names, or `None` for a string `typeof` never yields.
    pub(crate) fn from_literal(s: &str) -> Option<Self> {
        Some(match s {
            "undefined" => Self::Undefined,
            "object" => Self::Object,
            "boolean" => Self::Boolean,
            "number" => Self::Number,
            "bigint" => Self::BigInt,
            "string" => Self::String,
            "symbol" => Self::Symbol,
            "function" => Self::Function,
            _ => return None,
        })
    }

    pub(crate) fn of(i: &crate::interpreter::Interp, v: &Value) -> Self {
        match v {
            Value::Obj(_) if i.is_htmldda(v) => Self::Undefined,
            Value::Undefined | Value::Empty => Self::Undefined,
            Value::Null => Self::Object,
            Value::Bool(_) => Self::Boolean,
            Value::Num(_) => Self::Number,
            Value::BigInt(_) => Self::BigInt,
            Value::Str(_) => Self::String,
            Value::Sym(_) => Self::Symbol,
            Value::Obj(_) if v.is_callable() => Self::Function,
            Value::Obj(_) => Self::Object,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArithKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
}

impl ArithKind {
    fn of(op: &Op) -> Option<ArithKind> {
        Some(match op {
            Op::Add => ArithKind::Add,
            Op::Sub => ArithKind::Sub,
            Op::Mul => ArithKind::Mul,
            Op::Div => ArithKind::Div,
            Op::Mod => ArithKind::Mod,
            Op::BitAnd => ArithKind::BitAnd,
            Op::BitOr => ArithKind::BitOr,
            Op::BitXor => ArithKind::BitXor,
            Op::Shl => ArithKind::Shl,
            Op::Shr => ArithKind::Shr,
            Op::UShr => ArithKind::UShr,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            ArithKind::Add => "+",
            ArithKind::Sub => "-",
            ArithKind::Mul => "*",
            ArithKind::Div => "/",
            ArithKind::Mod => "%",
            ArithKind::BitAnd => "&",
            ArithKind::BitOr => "|",
            ArithKind::BitXor => "^",
            ArithKind::Shl => "<<",
            ArithKind::Shr => ">>",
            ArithKind::UShr => ">>>",
        }
    }
    #[inline(always)]
    fn num(self, a: f64, b: f64) -> f64 {
        use crate::eval::to_int32 as i32_;
        match self {
            ArithKind::Add => a + b,
            ArithKind::Sub => a - b,
            ArithKind::Mul => a * b,
            ArithKind::Div => a / b,
            ArithKind::Mod => crate::eval::js_mod(a, b),
            ArithKind::BitAnd => (i32_(a) & i32_(b)) as f64,
            ArithKind::BitOr => (i32_(a) | i32_(b)) as f64,
            ArithKind::BitXor => (i32_(a) ^ i32_(b)) as f64,
            ArithKind::Shl => i32_(a).wrapping_shl(i32_(b) as u32 & 31) as f64,
            ArithKind::Shr => (i32_(a) >> (i32_(b) as u32 & 31)) as f64,
            ArithKind::UShr => ((i32_(a) as u32) >> (i32_(b) as u32 & 31)) as f64,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpKind {
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    NotEq,
    StrictEq,
    StrictNotEq,
}

impl CmpKind {
    /// `===` / `!==`: never coerces, never calls user code.
    #[inline(always)]
    fn is_strict(self) -> bool {
        matches!(self, CmpKind::StrictEq | CmpKind::StrictNotEq)
    }
    /// The compare a standalone comparison op performs.
    fn of_op(op: &Op) -> Option<CmpKind> {
        Some(match op {
            Op::Lt => CmpKind::Lt,
            Op::Gt => CmpKind::Gt,
            Op::Le => CmpKind::Le,
            Op::Ge => CmpKind::Ge,
            Op::EqEq => CmpKind::EqEq,
            Op::NotEq => CmpKind::NotEq,
            Op::StrictEq => CmpKind::StrictEq,
            Op::StrictNotEq => CmpKind::StrictNotEq,
            _ => return None,
        })
    }
    fn of(op: &str) -> Option<CmpKind> {
        Some(match op {
            "<" => CmpKind::Lt,
            ">" => CmpKind::Gt,
            "<=" => CmpKind::Le,
            ">=" => CmpKind::Ge,
            "==" => CmpKind::EqEq,
            "!=" => CmpKind::NotEq,
            "===" => CmpKind::StrictEq,
            "!==" => CmpKind::StrictNotEq,
            _ => return None,
        })
    }
    fn name(self) -> &'static str {
        match self {
            CmpKind::Lt => "<",
            CmpKind::Gt => ">",
            CmpKind::Le => "<=",
            CmpKind::Ge => ">=",
            CmpKind::EqEq => "==",
            CmpKind::NotEq => "!=",
            CmpKind::StrictEq => "===",
            CmpKind::StrictNotEq => "!==",
        }
    }
    #[inline(always)]
    fn num(self, a: f64, b: f64) -> bool {
        match self {
            CmpKind::Lt => a < b,
            CmpKind::Gt => a > b,
            CmpKind::Le => a <= b,
            CmpKind::Ge => a >= b,
            CmpKind::EqEq | CmpKind::StrictEq => a == b,
            CmpKind::NotEq | CmpKind::StrictNotEq => a != b,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Const(u32),
    Undef,
    Dup,
    Pop,
    LoadLocal(u16),
    StoreLocal(u16),
    /// Read a captured local from the activation environment (TDZ-checked). The operand indexes
    /// `names`; the activation env holds exactly the captured bindings, so this is one hash hit.
    LoadCap(u32),
    /// Write a captured local (TDZ-checked: assignment before a lexical's initialization throws).
    StoreCap(u32),
    /// Initialize a captured lexical (`let`/`const` declaration): sets the value and clears TDZ.
    StoreCapInit(u32),
    /// `++`/`--` on a captured local, in place (the env-homed `UpdateLocal`).
    UpdateCap(u32, UpdKind),
    /// `++`/`--` on a free binding resolved through the closure/global environment.
    UpdateName(u32, UpdKind),
    /// [`Op::UpdateName`] with a per-site generation-checked name cache; misses retain the
    /// complete free-name/ToNumeric semantics.
    UpdateNameCached(u32, u32, UpdKind),
    /// Create a closure over the current environment from `Chunk::funcs[fidx]`. The second
    /// operand names an anonymous function expression per NamedEvaluation (`names` index, or
    /// `u32::MAX` for none).
    MakeClosure(u32, u32),
    /// `++`/`--` on a local slot, done in place (no LoadLocal/Plus/Add dance). Applies ToNumeric
    /// so a BigInt slot stays a BigInt — the `Plus`-based lowering this replaces was ToNumber and
    /// wrongly threw on BigInt. The `UpdKind` says increment vs decrement and which value (old,
    /// new, or none in statement position) to leave on the stack.
    UpdateLocal(u16, UpdKind),
    /// Put the slot into its temporal dead zone (block entry for `let`/`const`).
    Tdz(u16),
    /// Read a free name (resolved through the scope chain / global). Operands: name index,
    /// per-site [`NameIc`] index into `Chunk::name_caches`.
    LoadName(u32, u32),
    StoreName(u32),
    /// [`Op::StoreName`] with a per-site generation-checked name cache.
    StoreNameCached(u32, u32),
    LoadThis,
    /// Resolve lexical this at the read, preserving derived-constructor TDZ semantics.
    LoadLexicalThis,
    /// `obj.name`. First operand is the name index; second is the per-site inline-cache index into
    /// `Chunk::caches` (see `Interp::get_prop_ic`).
    GetProp(u32, u32),
    /// `this.name` — GetProp with the receiver read straight from the frame's `this` binding:
    /// no operand-stack traffic and no receiver refcounting (the frame owns the binding).
    GetPropThis(u32, u32),
    /// `local.name` — receiver read straight from a slot (alive for the whole frame). A TDZ
    /// slot throws the same ReferenceError LoadLocal would.
    GetPropLocal(u16, u32, u32),
    /// `obj.name = v`. Operands: name index, inline-cache index.
    SetProp(u32, u32),
    /// `obj.name = v` in statement position: stores without leaving `v` on the stack.
    SetPropDrop(u32, u32),
    /// `this.name = v` statement — SetPropDrop with the receiver from the frame's `this`
    /// binding: pops only the value.
    SetPropThisDrop(u32, u32),
    /// `local.name = v` statement — receiver from a slot; pops only the value. Only emitted
    /// when the RHS provably can't reassign the local (evaluation-order safety).
    SetPropLocalDrop(u16, u32, u32),
    /// Template-literal substitution: ToString (string hint) on the top of the stack.
    ToStr,
    /// `for…of` prologue: pops the iterable, pushes the iterator object then its `next` method
    /// (GetIterator with the sync hint, via the interpreter's own helper).
    GetIter,
    /// Snapshot for-in keys with the interpreter's enumeration rules.
    ForInKeys,
    /// Step a private key snapshot, skipping deleted properties. Base, keys, cursor slots.
    ForInStepL(u16, u16, u16),
    /// One `for…of` step against the iterator/next stored in the two slots: pushes the yielded
    /// value (or `undefined` at exhaustion) then a has-value bool — a following `JumpIfFalse`
    /// exits the loop, so the branch reuses existing machinery in both tiers. A `next`/`done`/
    /// `value` trap throw propagates as-is (the spec skips IteratorClose for step throws).
    IterStepL(u16, u16),
    /// IteratorClose on the iterator in the slot, normal-completion mode (close errors
    /// propagate): emitted where a `break`/`return` legitimately exits a compiled `for…of`.
    IterCloseL(u16),
    /// The `for…of` body's catch pad: pops the in-flight exception, closes the slot's iterator
    /// in throw mode (trap errors swallowed, per spec), and rethrows. Always abrupt.
    IterAbortL(u16),
    /// Object-destructuring guard: throws the oracle's TypeError when the value on top of the
    /// stack (peeked, not popped) is null/undefined — GetProp's own nullish error has a
    /// different message, and the check must run before any property read.
    DestructureGuard,
    /// Array-destructuring prologue: pops the iterable and walks the iterator protocol for `n`
    /// pattern elements (undefined once exhausted; IteratorClose when the pattern didn't drain
    /// it), pushing the `n` element values (last element on top — the stores emit in reverse,
    /// which is unobservable because only uncaptured slot leaves compile to this). Flat
    /// ident/hole elements only: a nested pattern's own reads would interleave with the
    /// iterator steps in the wrong order.
    DestructureArr(u16),
    /// `delete obj.name` (pops obj, pushes the bool result). Operands: name index and the
    /// *function's* strictness, which travels in the op rather than relying on the oracle's
    /// `self.strict`.
    DeleteProp(u32, bool),
    /// `delete obj[k]` (pops k then obj, pushes the bool result; ToPropertyKey on the key
    /// before the nullish-base check, matching the oracle's order).
    DeleteElem(bool),
    /// `f(a, b, ...c)` — a spread argument in the LAST position (the only shape whose
    /// evaluate-everything-then-expand lowering matches the spec's interleaved evaluation
    /// order): pops the iterable and `argc-1` plain arguments, expands via the iterator
    /// protocol, and calls through the generic path (no call IC — spread sites are cold).
    CallSpread(u16),
    /// [`Op::CallSpread`] with a receiver beneath the callee (method calls, with-object hits).
    CallSpreadThis(u16),
    /// Statement-position `obj.name += v` (pops v, the compound-read lval, obj): appends IN
    /// PLACE when the property still holds the exact string the read produced and everything is
    /// plain (see `Interp::append_prop_fast`); otherwise runs the generic Add + IC store —
    /// observably identical to the unfused GetProp/Add/SetPropDrop sequence.
    AppendProp(u32, u32),
    GetElem,
    SetElem,
    /// `obj[k] = v` in statement position: stores without leaving `v` on the stack.
    SetElemDrop,
    /// `x[k]` where `x` is a never-TDZ local slot (param or `var`): fused LoadLocal+GetElem —
    /// the receiver never crosses the operand stack (no clone/drop refcount churn). Reads the
    /// slot at exec time, which is only sound because the emitter proves the key expression
    /// cannot reassign the base local (see `Compiler::fused_elem_slot`) and the slot can never
    /// TDZ-throw.
    GetElemLocal(u16),
    /// `.length` of the virtual `arguments` object / rest array in slot `.0` (see
    /// [`Chunk::virt_base`]): the element count the slot holds, or the materialized object's.
    /// `.1` is the first element's slot, plus [`VIRT_REST`] for a rest array.
    ArgsLen(u16, u16),
    /// Pops a key and pushes that element of the virtual object in slot `.0` (operands as
    /// [`Op::ArgsLen`]): an in-range index reads its slot, any other key materializes the object.
    ArgsGet(u16, u16),
    /// `f.apply(t, arguments)` with `arguments` the virtual object in slot `.0` (operands as
    /// [`Op::ArgsLen`]): pops `[f, apply, t]` (the `GetMethod` receiver and method, then `t`)
    /// and pushes the result. The intrinsic `apply` on a callable `f` calls it with the
    /// arguments straight from their slots; anything else materializes the object and calls
    /// `apply` as written.
    ApplyArgs(u16, u16),
    /// `x[k] = v` with `x` a never-TDZ local slot (see [`Op::GetElemLocal`]), keeping `v`.
    SetElemLocal(u16),
    /// `x[k] = v` in statement position with `x` a never-TDZ local slot.
    SetElemLocalDrop(u16),
    /// `obj.name++` / `--obj.name` as one op: pops obj, reads via the site IC, ToNumeric, ±1,
    /// writes back via the IC, pushes old / new / nothing per `UpdKind`.
    UpdateProp(u32, u32, UpdKind),
    /// `obj[k]++` / `--obj[k]`: pops k and obj, coerces the key at most once (matching the
    /// oracle's cached-Reference semantics), read-modify-write, pushes per `UpdKind`.
    UpdateElem(UpdKind),
    /// Compound `obj[k] op= v` support: coerce the top of stack to a property key *now* when the
    /// coercion could be observable (an object's valueOf/toString), so the following GetElem +
    /// SetElem pair can't run it twice. Num/Str keys stay raw — their later coercion is
    /// side-effect-free and deterministic, and keeping numbers numeric preserves the dense-array
    /// fast path. Checks the base (one below top) for null/undefined first, like `ref_prop_key`.
    ToPropKey,
    /// [`Op::ToPropKey`] for the slot-fused compound form: the base is read from the local slot
    /// (never on the stack), keys already Num/Str pass through untouched.
    ToPropKeyLocal(u16),
    /// Duplicate the top two stack values (for compound `obj[k] op= v`).
    Dup2,
    /// `obj.name` as a call target: pops obj, pushes obj then the method (get runs before args).
    /// Operands: name index, inline-cache index (methods live on prototypes — the IC walks hops).
    GetMethod(u32, u32),
    /// `obj[k]` as a call target: pops k and obj, pushes obj then the method.
    GetMethodElem,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    NotEq,
    StrictEq,
    StrictNotEq,
    /// `lhs instanceof rhs`, with one shape cache for the overwhelmingly common ordinary
    /// constructor case; misses retain the complete `@@hasInstance` semantics through the generic executor.
    InstanceOf(u32),
    /// Any other binary operator (`**`, `in`, `instanceof`) via the interpreter, op in names.
    GenBin(u32),
    Neg,
    Plus,
    Not,
    BitNot,
    Typeof,
    /// `typeof v === "<kind>"` (or `!==` when the flag is set; loose and strict equality agree
    /// on two strings): pops `v`, pushes a Bool. No result string is built or compared.
    TypeofIs(TypeofKind, bool),
    /// `typeof freeName`: absent bindings yield "undefined", while lexical TDZ still throws.
    TypeofName(u32),
    Void,
    Jump(u32),
    JumpIfFalse(u32),
    /// `a <cmp> b` then `JumpIfFalse`, fused: no boolean pushed and popped (a store-forwarding
    /// stall on the 16-byte stack slots) and one dispatch fewer — the shape of every loop test.
    JumpIfNotCmp(CmpKind, u32),
    /// [`Op::JumpIfNotCmp`] on two locals, read in place (TDZ-checked like `LoadLocal`).
    JumpIfNotCmpLL(CmpKind, u16, u16, u32),
    /// [`Op::JumpIfNotCmp`] on a local and a constant: `k < n` loop tests touch no stack.
    JumpIfNotCmpLK(CmpKind, u16, u32, u32),
    /// `LoadLocal(a) LoadLocal(b) <arith> StoreLocal(dst)`, fused by [`peephole`]: `(dst, a, b)`.
    ArithLL(ArithKind, u16, u16, u16),
    /// `LoadLocal(a) Const(k) <arith> StoreLocal(dst)`, fused by [`peephole`]: `(dst, a, k)`.
    ArithLK(ArithKind, u16, u16, u32),
    /// Peek variants leave the operand on the stack (for `&&` / `||` / `??`).
    JumpIfFalsePeek(u32),
    JumpIfTruePeek(u32),
    JumpIfNotNullishPeek(u32),
    /// Plain call: pops argc args and the callee; `this` is undefined.
    Call(u16),
    /// Resolve a free name as a call target *before* the arguments evaluate (spec order):
    /// pushes the `with`-object `this` (or undefined) then the callee, feeding CallWithThis.
    /// Operands: name index, per-site [`NameIc`] index.
    LoadNameForCall(u32, u32),
    /// Method call: pops argc args, the method, and the receiver pushed by GetMethod*.
    CallWithThis(u16),
    New(u16),
    /// A regexp literal: source and flags indices in `names`. Each execution allocates a fresh
    /// JS RegExp object while `Interp::regexp_programs` shares the immutable compiled matcher.
    MakeRegExp(u32, u32),
    MakeArray(u16),
    /// Object literal: `count` plain data keys starting at names[start], values on the stack.
    MakeObject(u32, u16, u32),
    Throw,
    Return,
    ReturnUndef,
    /// `await expr`: suspend the async body, handing the popped operand to the driver; on resume the
    /// settled value is pushed back (or a rejection is thrown). Only emitted for async functions.
    Await,
    /// Enter a `try` region: register a handler that, on a throw anywhere in the region, unwinds the
    /// stack and jumps to the operand (the catch pc, with the exception pushed).
    PushHandler(u32),
    /// Leave a `try` region without throwing: drop the innermost handler.
    PopHandler,
    // ---- Widened subset (lowerings and handlers in `bytecode/ext_ops.rs`) ----
    /// `obj.#x`: pops obj, resolves the private name (names index) through the env chain,
    /// pushes PrivateGet's result (brand-checked).
    GetPrivate(u32),
    /// `obj.#x = v`: pops v and obj, PrivateSet, pushes v.
    SetPrivate(u32),
    /// `obj.#m` as a call target: pops obj, pushes obj then the private method.
    GetPrivateMethod(u32),
    /// `#x in obj`: pops obj, pushes the brand-check bool (TypeError on a non-object).
    PrivateIn(u32),
    /// `obj.#x++` and friends: pops obj, ToNumeric/±1 via PrivateGet/PrivateSet, pushes per kind.
    UpdatePrivate(u32, UpdKind),
    /// Object literal (general form): push a fresh ordinary object.
    NewObject,
    /// `key: v` on the literal beneath: pops v (the object stays). The flag applies the oracle's
    /// anonymous-function naming (`set_fn_name` after evaluation).
    InitProp(u32, bool),
    /// `[k]: v`: pops v and the (ToPropKey'd) key; the object stays.
    InitPropComputed(bool),
    /// Concise method / getter / setter (kind 0/1/2) from `funcs[fidx]` with the literal as its
    /// [[HomeObject]]; key from names (or popped, when the name operand is `u32::MAX`).
    InitMethod(u32, u32, u16),
    /// `...src` in an object literal: pops src, CopyDataProperties into the object beneath.
    CopyDataProps,
    /// `__proto__: v` in an object literal: pops v, sets the prototype when Object/Null.
    SetProtoLit,
    /// ClassDefinitionEvaluation of `classes[cidx]` over the current env (the oracle's own
    /// routine); the second operand is the NamedEvaluation name (`u32::MAX` = none).
    MakeClass(u32, u32),
    /// Drop the value beneath the top of stack (`[a, b]` → `[b]`).
    Nip,
    /// `super.name` (pops the this value pushed before it).
    SuperGet(u32),
    /// `super[k]` (pops k, then the this value).
    SuperGetElem,
    /// Push the current super base (the [[HomeObject]]'s prototype, from the env chain).
    SuperBase,
    /// `super.name(...)` target: pops the super base, reads the method from it, pushes the
    /// `this` value (lexical when the flag is set) then the method.
    SuperMethod(u32, bool),
    /// `super[k](...)` target: pops k and the super base; like [`Op::SuperMethod`].
    SuperMethodElem(bool),
    /// Object-pattern rest: peeks the destructured value, pushes the CopyDataProperties copy
    /// excluding the `count` keys at names[start..].
    ObjRest(u32, u16),
    /// Array literal with spreads or holes: pushes a fresh empty array (see `ArrayAppend`).
    NewArrayLit,
    /// Pops v, appends it to the array literal beneath (CreateDataProperty at its length).
    ArrayAppend,
    /// Pops an iterable, appends every value it yields (the oracle's `iterate`).
    ArrayAppendSpread,
    /// An elision: grows the array literal beneath by one, leaving the index absent.
    ArrayHole,
    /// Inlined array callback guard (see [`inline_callback`]): `[O, F]` → `[O, F, ok]`,
    /// ok = the method call may run as the inlined loop (`CbMethod` operand). Never throws.
    ArrayCbGuard(u8),
    /// Inlined callback element step after `Get` produced `undefined`: pops k and the array,
    /// pushes HasProperty(array, k).
    ArrayCbHas,
    /// End of an inlined callback loop: publishes the method call's source position as the
    /// frame's call site (as the returned builtin call would have).
    ArrayCbDone(u32),
    /// A `switch` case chain's dispatch table (see [`switch_table`]): reads local `slot`, jumps
    /// to the matching case's body or past the chain's covered prefix, or falls through into
    /// the (unchanged) chain when the table can't decide. Operands: slot, table index into
    /// `Chunk::switch_tables`. Inserted by [`switch_table::switch_pass`] only.
    SwitchLK(u16, u32),
    // ---- Derived constructors and generators (`bytecode/derived.rs`, `bytecode/generator.rs`) ----
    /// `super(…)` prologue (derived-mode chunks): the super-call checks and GetSuperConstructor;
    /// pushes the parent constructor, then the `this` it will construct on.
    SuperCtor,
    /// `super(a, b)`: pops the arguments, the `this` and the parent pushed by `SuperCtor`, runs
    /// the parent construct / BindThisValue / field initializers, pushes the bound `this`.
    SuperCall(u16),
    /// [`Op::SuperCall`] whose last argument is a spread (expanded via the iterator protocol).
    SuperCallSpread(u16),
    /// A derived constructor's `return` (and implicit end-of-body return): pops the value and
    /// applies the [[Construct]] rule — an object wins, undefined yields the (TDZ-checked) `this`
    /// binding, anything else is a TypeError. The errors are raised past every handler of the
    /// frame (they belong to [[Construct]], after the body completed).
    DerivedReturn,
    /// Generator prologue end: the call parks here (suspendedStart) — see [`generator`].
    InitialYield,
    /// `yield v`: pops `v` and parks; the resume pushes `[received, returning]`.
    Yield,
    /// `yield*` over the inner iterator in slots `(it, it + 1)`: consumes `[received, mode]`,
    /// then finishes pushing `[value, returning]` or parks on itself (see [`generator`]).
    YieldDelegate(u16),
    /// Push the running function object (the innermost `FnFrame`'s callee): a named function
    /// expression's self-name binding, initialized in the prologue.
    LoadCallee,
    /// Push `new.target` of the running (non-arrow, synchronous) function: the engine's
    /// current value, which every call path sets on entry (the constructor on a construct,
    /// `undefined` on a call) and restores on exit.
    LoadNewTarget,
    /// A template literal's concatenation: pops `n` Strings (its literal chunks and ToStr'd
    /// substitutions, in order), pushes their concatenation.
    Concat(u16),
    /// A class instance field's DefineField (see [`class_fields`]): pops the value and the
    /// receiver, defines own data property `names[n]` (a resolved private name adds a private
    /// field). The flag applies the anonymous-function naming.
    DefineField(u32, bool),
    /// Captured block-scoped bindings (see [`block_env`]): a fresh block env in slot `.0`
    /// whose parent is the block env in slot `.1` (`u16::MAX` = the frame env).
    BlkNew(u16, u16),
    /// Declare binding `names[.1]` (in TDZ; `.2` = const) in the block env of slot `.0`.
    BlkDecl(u16, u32, bool),
    /// CreatePerIterationEnvironment for the block env in slot `.0`.
    BlkCopy(u16),
    /// Push block binding `names[.1]` of slot `.0`'s env (TDZ-checked).
    BlkLoad(u16, u32),
    /// Pop and assign block binding `names[.1]` (TDZ and const checked).
    BlkStore(u16, u32),
    /// Pop and initialize block binding `names[.1]`.
    BlkInit(u16, u32),
    /// `++`/`--` on block binding `names[.1]`.
    BlkUpdate(u16, u32, UpdKind),
    /// Run the next op (`MakeClosure` / `MakeClass` / `InitMethod`) with slot `.0`'s block env
    /// as the created function's scope.
    InEnv(u16),
    /// A call in tail position of strict code (see [`self_tail`]): like `Call(n)` /
    /// `CallWithThis(n)` (`.1`), always followed by `Return`. When the frame may tail-call
    /// (`Interp::tco_ok`) the current frame is released before the callee runs — proper tail
    /// calls in constant space.
    TailCall(u16, bool),
    /// [`Op::TailCall`] for `CallSpread(n)` / `CallSpreadThis(n)` (`.1`).
    TailCallSpread(u16, bool),
    /// `import(spec[, options])`: pops the options (when `.1`) and the specifier, pushes the
    /// promise. `.0` is the phase (0 evaluation, 1 source, 2 defer).
    ImportCall(u8, bool),
    /// `for await` head (see [`for_await`]): pops the iterable, pushes iterator, `next`, state.
    GetAsyncIter,
    /// `for await` step: calls `next()` on the iterator/next/state slots, pushes the value the
    /// following `Await` awaits.
    AsyncIterNext(u16, u16, u16),
    /// Pops the awaited step, pushes (value, has-value) like `IterStepL`. Operand: state slot.
    AsyncIterResult(u16),
    /// AsyncIteratorClose's call (iterator, state slots): pushes `false`, or the value to
    /// await and `true`.
    AsyncCloseCall(u16, u16),
    /// Pops the awaited `return()` result and checks it (state slot).
    AsyncCloseCheck(u16),
    /// Async generator `yield*` (see [`generator`]): GetIterator(async) — pops the operand,
    /// pushes iterator, `next`, from-sync flag.
    AsyncDelegateInit,
    /// Pops the received value and forwards it per the mode slot (iterator, mode, return-flag
    /// slots): pushes the inner result and `true`, or a special completion's value and `false`.
    AsyncDelegateCall(u16, u16, u16),
    /// Pops the awaited inner result: records done (slot .1), pushes its value and the
    /// from-sync flag (slot .0).
    AsyncDelegateResult(u16, u16),
    /// A from-sync value's rejection pad (iterator, done slots). Always abrupt.
    AsyncDelegateCloseReject(u16, u16),
    /// After the special path's await (mode slot; `true` = the rejection pad).
    AsyncDelegateSpecial(u16, bool),
    /// `new C(a, ...rest)`: like [`Op::CallSpread`] (the final argument is spread) for `New`.
    NewSpread(u16),
    /// Array-destructuring rest: pushes the remaining values of the iterator/next in the two
    /// slots as a fresh array (see [`destructure_seq`]).
    IterRestL(u16, u16),
}

/// An active `try` region on the VM's handler stack.
struct Handler {
    /// Where to jump on a throw (the catch entry).
    catch_pc: usize,
    /// The operand-stack depth to unwind to before pushing the exception.
    stack_depth: usize,
}

/// How one captured binding seeds into the activation environment at entry (in order).
#[derive(PartialEq, Eq)]
pub(crate) enum CapInit {
    /// A captured parameter: seed from argument `k`.
    Param(u16, Rc<str>),
    /// A captured function-scoped `var`: undefined, unless already bound (a same-named param).
    Var(Rc<str>),
    /// A hoisted function declaration: a closure over the activation itself (self-recursion).
    Fn(u16, Rc<str>),
    /// A captured top-level lexical: inserted uninitialized (TDZ); bool = `const`.
    Lexical(Rc<str>, bool),
}

pub struct Chunk {
    /// Diagnostic identity; not part of the portable bytecode format.
    debug_name: String,
    // (fields below; Debug is manual — `consts` holds engine Values)
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    n_slots: usize,
    /// Slot names, for TDZ ReferenceError messages.
    slot_names: Vec<Rc<str>>,
    /// Parameter positions map onto slots [0, n_params).
    n_params: usize,
    /// Slot holding the ordinary function's materialized `arguments` object. Only compiled for
    /// parameterless synchronous functions for now, avoiding mapped-parameter aliasing while
    /// covering common variadic helpers.
    arguments_slot: Option<u16>,
    /// Slots reset to undefined after parameter seeding (the tree-walker's `for`-head var
    /// hoisting overwrites same-named params; replicated bug-for-bug — it is the oracle).
    var_force_resets: Vec<u16>,
    uses_this: bool,
    /// Inner function templates for `MakeClosure`.
    funcs: Vec<Rc<Function>>,
    /// Class definitions for `MakeClass` (a chunk holding any is not serialized).
    classes: Vec<Rc<Class>>,
    /// The rest parameter's slot: seeded with an array of the arguments past `n_params`.
    rest_slot: Option<u16>,
    /// A virtual `arguments` object (with `arguments_slot`) or rest array (with `rest_slot`):
    /// the body only reads its `length` and elements ([`Op::ArgsLen`], [`Op::ArgsGet`]) and
    /// writes no parameter, so the arguments stay in the parameter slots — `n_params` is widened
    /// by a window of hidden ones — and the object's slot holds just the element count, whose
    /// elements start at this slot. A call with more arguments than `n_params` materializes the
    /// object into the slot instead.
    pub(crate) virt_base: Option<u16>,
    /// Captured bindings to seed into a fresh activation env at entry; empty = no activation
    /// needed (closures, if any, capture the definition env directly).
    cap_inits: Vec<CapInit>,
    activation_layout: Option<activation::ActivationLayout>,
    /// An inner arrow chain reads the outer `this`: the activation carries a `this` binding.
    env_this: bool,
    /// One inline-cache slot per property-access op (`GetProp`/`SetProp`/`SetPropDrop`/`GetMethod`),
    /// holding the (prototype depth, `entries` slot) last seen for that site (see [`IcState`]). The
    /// `Chunk` is shared across calls via `Rc`, so these persist. `Cell` is fine: the VM runs one
    /// thread at a time (coroutine ping-pong), like the rest of the engine's shared-`Rc` state.
    caches: Vec<std::cell::Cell<IcState>>,
    /// Optimizing-tier state: loop hotness and native code (see [`jit`]).
    jit: jit::ChunkJit,
    #[cfg(feature = "bench")]
    pub(crate) precompiled_origin: Option<(usize, usize)>,
    /// One pre-shaped `Props` template per plain object-literal site (`Op::MakeObject`'s third
    /// operand indexes this; `u32::MAX` = duplicate keys, take the insert path). Built on first
    /// execution, cloned per instance — key hashing and shape transitions paid once per SITE.
    obj_maps: Vec<std::cell::OnceCell<crate::value::Props>>,
    /// One [`NameIc`] slot per free-name op (`LoadName`/`LoadNameForCall`), persisting across
    /// calls like `caches`.
    name_caches: Vec<std::cell::Cell<NameIc>>,
    name_paths: Vec<std::cell::RefCell<Option<name_path::NamePath>>>,
    /// Weak handles pinning each name cache's scope allocation (parallel to `name_caches`), so
    /// the cached raw `env` pointer can never be recycled into a different scope while cached.
    name_pins: std::cell::RefCell<
        Vec<Option<std::rc::Weak<std::cell::RefCell<crate::interpreter::Scope>>>>,
    >,
    /// Captured-binding cache keyed by the chunk's interned-name index. Unlike a free-name cache,
    /// this always resolves in the current activation; a weak pin keeps its raw scope pointer
    /// ABA-safe until a fresh/recursive activation refills the entry.
    cap_caches: Vec<std::cell::Cell<NameIc>>,
    cap_pins: std::cell::RefCell<
        Vec<Option<std::rc::Weak<std::cell::RefCell<crate::interpreter::Scope>>>>,
    >,
    /// One lazily built table per [`Op::SwitchLK`] (its second operand).
    switch_tables: Vec<std::cell::OnceCell<switch_table::SwitchTable>>,
    /// Compiled in derived-constructor mode (see [`derived`]): `this` is a TDZ activation
    /// binding and returns apply the derived [[Construct]] rule. Only such a chunk may run a
    /// derived class construct.
    derived: bool,
    /// A sloppy ordinary function (not an arrow, method, generator or async body): the legacy
    /// `f.arguments` / `f.caller` reflection sees its activations (see [`reflect`]).
    reflect_args: bool,
    /// The source positions of the call sites, for stack traces (see [`positions`]).
    positions: Box<[u8]>,
    /// Inlined array-callback regions, scanned from `ops` on first use by a stack trace (see
    /// [`inline_callback`]).
    inline_cbs: std::cell::OnceCell<Box<[inline_callback::InlineRegion]>>,
    #[cfg(feature = "compiler")]
    feedback_key: std::cell::OnceCell<u64>,
}

impl Chunk {
    #[cfg(feature = "compiler")]
    pub(crate) fn native_equivalent(&self, other: &Self) -> bool {
        let source_equal = |left: &crate::ast::FnSource, right: &crate::ast::FnSource| {
            left.as_str()
                .zip(right.as_str())
                .is_some_and(|(left, right)| left == right)
        };
        self.debug_name == other.debug_name
            && self.ops == other.ops
            && self.names == other.names
            && self.n_slots == other.n_slots
            && self.slot_names == other.slot_names
            && self.n_params == other.n_params
            && self.arguments_slot == other.arguments_slot
            && self.rest_slot == other.rest_slot
            && self.virt_base == other.virt_base
            && self.var_force_resets == other.var_force_resets
            && self.cap_inits == other.cap_inits
            && self.uses_this == other.uses_this
            && self.env_this == other.env_this
            && self.derived == other.derived
            && self.reflect_args == other.reflect_args
            && self.positions == other.positions
            && self.caches.len() == other.caches.len()
            && self.name_caches.len() == other.name_caches.len()
            && self.obj_maps.len() == other.obj_maps.len()
            && self.consts.len() == other.consts.len()
            && self
                .consts
                .iter()
                .zip(&other.consts)
                .all(|(left, right)| match (left, right) {
                    (Value::Undefined, Value::Undefined)
                    | (Value::Empty, Value::Empty)
                    | (Value::Null, Value::Null) => true,
                    (Value::Bool(left), Value::Bool(right)) => left == right,
                    (Value::Num(left), Value::Num(right)) => left.to_bits() == right.to_bits(),
                    (Value::Str(left), Value::Str(right)) => left.as_str() == right.as_str(),
                    (Value::BigInt(left), Value::BigInt(right)) => left == right,
                    (Value::Sym(left), Value::Sym(right)) => Rc::ptr_eq(left, right),
                    (Value::Obj(left), Value::Obj(right)) => Gc::ptr_eq(left, right),
                    _ => false,
                })
            && self.funcs.len() == other.funcs.len()
            && self.funcs.iter().zip(&other.funcs).all(|(left, right)| {
                Rc::ptr_eq(left, right)
                    || (source_equal(&left.source, &right.source)
                        && left.name == right.name
                        && left.is_arrow == right.is_arrow
                        && left.is_method == right.is_method
                        && left.is_strict == right.is_strict
                        && left.is_generator == right.is_generator
                        && left.is_async == right.is_async
                        && left.is_fn_expr == right.is_fn_expr)
            })
            && self.classes.len() == other.classes.len()
            && self
                .classes
                .iter()
                .zip(&other.classes)
                .all(|(left, right)| {
                    Rc::ptr_eq(left, right) || source_equal(&left.source, &right.source)
                })
    }
    /// Lower this host-side chunk to complete boxed native control flow.
    #[cfg(feature = "jit")]
    pub fn build_native(&self, pointer_width: u8) -> Result<NativeFunction, String> {
        jit::aot::build(self, pointer_width)
    }

    #[cfg(all(feature = "jit", feature = "compiler"))]
    pub fn build_native_with_profile(
        &self,
        pointer_width: u8,
        profile: &crate::feedback::Profile,
    ) -> Result<NativeFunction, String> {
        jit::aot::build_with_profile(self, pointer_width, Some(profile))
    }

    #[cfg(feature = "compiler")]
    pub(crate) fn feedback_key(&self) -> u64 {
        *self.feedback_key.get_or_init(|| {
            let mut key = crate::feedback::hash(self.debug_name.as_bytes(), lumen_common::fasthash::FNV1A64_OFFSET);
            key = crate::feedback::hash(format!("{:?}", self.ops).as_bytes(), key);
            for name in &self.names {
                key = crate::feedback::hash(name.as_bytes(), key);
            }
            for value in &self.consts {
                key =
                    crate::feedback::hash(&crate::native_ops::value_kind(value).to_le_bytes(), key);
                match value {
                    Value::Num(n) => key = crate::feedback::hash(&n.to_bits().to_le_bytes(), key),
                    Value::Bool(b) => key = crate::feedback::hash(&[u8::from(*b)], key),
                    Value::Str(s) => key = crate::feedback::hash(s.to_string().as_bytes(), key),
                    Value::BigInt(b) => {
                        key = crate::feedback::hash(b.to_string_radix(10).as_bytes(), key)
                    }
                    _ => {}
                }
            }
            key
        })
    }

    /// Host compiler metadata. Never serialize the chunk itself in a native image.
    #[cfg(feature = "jit")]
    pub fn native_constants(&self) -> &[Value] {
        &self.consts
    }
    #[cfg(feature = "jit")]
    pub fn native_names(&self) -> &[Rc<str>] {
        &self.names
    }
    #[cfg(feature = "jit")]
    pub fn native_slot_names(&self) -> &[Rc<str>] {
        &self.slot_names
    }
    #[cfg(feature = "jit")]
    pub fn native_slot_count(&self) -> usize {
        self.n_slots
    }
    #[cfg(feature = "jit")]
    pub fn native_parameter_count(&self) -> usize {
        self.n_params
    }
    #[cfg(feature = "jit")]
    pub fn native_child_functions(&self) -> &[Rc<Function>] {
        &self.funcs
    }
    #[cfg(feature = "jit")]
    pub fn native_classes(&self) -> &[Rc<Class>] {
        &self.classes
    }
    #[cfg(feature = "jit")]
    pub fn native_positions(&self) -> Vec<(u32, u32)> {
        self.ops
            .iter()
            .enumerate()
            .filter_map(|(pc, op)| {
                if !positions::is_site(op) {
                    return None;
                }
                let position = self.call_site_pos(pc);
                (position != crate::ast::NO_POS).then_some((pc as u32, position))
            })
            .collect()
    }
    #[cfg(feature = "jit")]
    pub fn native_frame_layout(&self) -> NativeFrameLayout<'_> {
        NativeFrameLayout {
            arguments_slot: self.arguments_slot,
            rest_slot: self.rest_slot,
            virt_base: self.virt_base,
            var_force_resets: &self.var_force_resets,
            cap_inits: self
                .cap_inits
                .iter()
                .map(|init| match init {
                    CapInit::Param(slot, name) => NativeCaptureInit::Param(*slot, name.clone()),
                    CapInit::Var(name) => NativeCaptureInit::Var(name.clone()),
                    CapInit::Fn(index, name) => NativeCaptureInit::Function(*index, name.clone()),
                    CapInit::Lexical(name, immutable) => {
                        NativeCaptureInit::Lexical(name.clone(), *immutable)
                    }
                })
                .collect(),
            uses_this: self.uses_this,
            env_this: self.env_this,
            derived: self.derived,
            reflect_args: self.reflect_args,
            property_cache_count: self.caches.len(),
            name_cache_count: self.name_caches.len(),
            object_template_count: self.obj_maps.len(),
        }
    }

    /// The source position of the call op at `pc` (`ast::NO_POS` if none is known).
    pub(crate) fn call_site_pos(&self, pc: usize) -> u32 {
        positions::lookup(&self.ops, &self.positions, pc)
    }

    /// Compiled in derived-constructor mode (see [`derived`]).
    pub(crate) fn is_derived(&self) -> bool {
        self.derived
    }

    /// Whether the body (or an inner arrow chain) reads `this`, so the caller must bind it.
    pub fn uses_this(&self) -> bool {
        self.uses_this || self.env_this
    }

    /// Seed the rest parameter's slot (if any) from the arguments past the positional ones.
    #[inline]
    fn seed_rest(&self, i: &mut Interp, slots: &mut [Value], args: &[Value]) {
        if let Some(base) = self.virt_base {
            let s = self
                .arguments_slot
                .or(self.rest_slot)
                .expect("virtual object slot");
            let p = self.n_params;
            slots[s as usize] = if args.len() <= p {
                Value::Num(args.len().saturating_sub(base as usize) as f64)
            } else {
                // The leading arguments may have been moved into the slots (still unmodified).
                let vals: Vec<Value> = slots[base as usize..p]
                    .iter()
                    .chain(&args[p..])
                    .cloned()
                    .collect();
                virt_materialize(i, self.rest_slot.is_some(), &vals)
            };
            return;
        }
        if let Some(s) = self.rest_slot {
            // One allocation (a single box for short lists) straight from the argument slice.
            let rest = args.get(self.n_params..).unwrap_or(&[]);
            slots[s as usize] = i.make_array_iter(rest.iter().cloned());
        }
    }

    /// Whether calls need a real activation environment (captured locals / lexical `this`).
    fn makes_env(&self) -> bool {
        !self.cap_inits.is_empty() || self.env_this
    }

    /// Whether the frame is observable beyond its slots: an activation environment, or an
    /// `arguments` object (which observes the call frame without needing an activation scope).
    fn needs_env(&self) -> bool {
        self.makes_env() || self.arguments_slot.is_some()
    }

    /// Build the activation environment for one call: a fresh scope under `env` holding exactly
    /// the captured bindings (and `this` when an inner arrow chain reads it). Free names and
    /// `MakeClosure` environments route through it. Returns `env` untouched when nothing is
    /// captured — closures then capture the definition env directly, which resolves identically
    /// because none of their free names are outer locals.
    fn make_run_env(&self, i: &Interp, env: &Env, this_val: &Value, args: &[Value]) -> Env {
        match &self.activation_layout {
            Some(layout) => layout.make_env(self, i, env, this_val, args),
            None => env.clone(),
        }
    }
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Chunk({} ops, {} slots)", self.ops.len(), self.n_slots)
    }
}

#[cfg(feature = "compiler")]
include!("bytecode/compiler.rs");
#[cfg(not(feature = "compiler"))]
pub fn compile(_func: &Function) -> Option<Rc<Chunk>> {
    None
}
#[cfg(not(feature = "compiler"))]
pub fn compile_derived(_func: &Function) -> Option<Rc<Chunk>> {
    None
}
#[cfg(all(feature = "parallel", not(feature = "compiler")))]
pub(crate) fn function_free_identifiers(
    _func: &Function,
) -> Option<std::collections::HashSet<String>> {
    None
}
// ---------------------------------------------------------------------------------------------
// VM
// ---------------------------------------------------------------------------------------------

/// How one run of the VM ended: the body returned a value, suspended at an `await` (async bodies
/// only — see [`VmCoro`]), or — carried as `Err(Abrupt::Throw)` — threw.
pub enum VmStep {
    Done(Value),
    Await(Value),
}

#[cfg(not(feature = "compiler"))]
pub fn run(i: &mut Interp, _: &Chunk, _: &Env, _: Value, _: &[Value]) -> Result<Value, Abrupt> {
    Err(i.throw(
        "EvalError",
        "bytecode execution is not available in an AOT build",
    ))
}

#[cfg(not(feature = "compiler"))]
#[allow(clippy::too_many_arguments)]
fn drive_vm(
    i: &mut Interp,
    _: &Chunk,
    _: &Env,
    _: &mut [Value],
    _: &mut Vec<Value>,
    _: &mut usize,
    _: &Value,
    _: &mut Vec<Handler>,
    _: Option<Value>,
) -> Result<VmStep, Abrupt> {
    Err(i.throw(
        "EvalError",
        "bytecode execution is not available in an AOT build",
    ))
}

/// Execute a compiled function body. `env` is the root for free-name resolution — the *definition*
/// environment when called leanly (see `Interp::call_user_inner`), since a compiled body has no
/// observable activation. Parameters seed straight into slots; `this_val` is the already-bound
/// `this` (computed only when the body reads it). Synchronous bodies only — an async body runs
/// through [`VmCoro`], which drives [`run_vm`] and can suspend it.
///
/// The slot and operand-stack buffers come from a per-interpreter pool ([`Interp::vm_pool`]) so a
/// hot call tree does not allocate two `Vec`s per call.
#[cfg(feature = "compiler")]
pub fn run(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    this_val: Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    // Captured locals (and a lexically-read `this`) live in a per-call activation env; slots
    // hold everything else. No captures → the definition env is used directly.
    let env = chunk.make_run_env(i, env, &this_val, args);
    let (mut slots, mut stack) = i.vm_pool.pop().unwrap_or_default();
    let seed = chunk.n_params.min(args.len());
    slots.extend_from_slice(&args[..seed]);
    slots.resize(chunk.n_slots, Value::Undefined);
    if let (Some(s), None) = (chunk.arguments_slot, chunk.virt_base) {
        slots[s as usize] = Value::Obj(i.make_compiled_arguments_object(args, &env));
    }
    if chunk.reflect_args && reflect::enabled() {
        reflect::stash_compiled(i, args, &[], slots.as_ptr());
    }
    chunk.seed_rest(i, &mut slots, args);
    for &s in &chunk.var_force_resets {
        slots[s as usize] = Value::Undefined;
    }
    let mut pc = 0usize;
    let mut handlers: Vec<Handler> = Vec::new();
    let r = drive_vm(
        i,
        chunk,
        &env,
        &mut slots,
        &mut stack,
        &mut pc,
        &this_val,
        &mut handlers,
        None,
    );
    slots.clear();
    stack.clear();
    if i.vm_pool.len() < 64 {
        i.vm_pool.push((slots, stack));
    }
    match r? {
        VmStep::Done(v) => Ok(v),
        VmStep::Await(_) => unreachable!("a synchronous bytecode function cannot await"),
    }
}

/// Drive the VM through throws: run to `Done`/`Await`, or on an uncaught op throw unwind to the
/// innermost `try` handler (restore its stack depth, push the exception, jump to its catch) and
/// keep going — propagating only when no handler remains. `pending_throw` injects a throw *before*
/// the first step: a rejected `await` resuming inside a `try` (see [`VmCoro::resume`]).
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "compiler")]
fn drive_vm(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    handlers: &mut Vec<Handler>,
    mut pending_throw: Option<Value>,
) -> Result<VmStep, Abrupt> {
    // --- whole-function JIT tier: a frame starting at pc 0 may run native function code ---
    if *pc == 0 && pending_throw.is_none() && stack.is_empty() && jit::entry_due(i, chunk) {
        match jit::on_entry(i, chunk, env, slots, stack, pc, this_val) {
            Ok(Some(step)) => return Ok(step),
            Ok(None) => {}
            Err(Abrupt::Throw(e)) => pending_throw = Some(e),
            Err(other) => return Err(other),
        }
    } else if pending_throw.is_none() && handlers.is_empty() && jit::resume_due(i, chunk, *pc) {
        // An async body resumed after an `await`: its function code continues there.
        match jit::on_resume(i, chunk, env, slots, stack, pc, this_val) {
            Ok(Some(step)) => return Ok(step),
            Ok(None) => {}
            Err(Abrupt::Throw(e)) => pending_throw = Some(e),
            Err(other) => return Err(other),
        }
    }
    // --- end whole-function JIT tier ---
    // Compiled callees the body calls run as inline frames on this same loop (see
    // [`InlineFrame`]); the record vector is only taken from the pool on the first such call.
    let mut frames = InlineFrames {
        recs: Vec::new(),
        live: 0,
    };
    let r = loop {
        let outcome = match pending_throw.take() {
            Some(e) => Err(Abrupt::Throw(e)),
            None => run_vm_frames::<false>(
                i,
                chunk,
                env,
                slots,
                stack,
                pc,
                this_val,
                handlers,
                &mut frames,
            ),
        };
        match outcome {
            // The innermost inline frame returned: hand the value to its caller and continue there.
            Ok(VmStep::Done(v)) if frames.live != 0 => {
                let v = construct_result(i, &mut frames, v);
                leave_inline(i, &mut frames);
                // Native code of the frame parked a proper tail call (see `TAIL_NEST`): the
                // caller makes it.
                let v = match i.pending_tail.take() {
                    None => v,
                    Some(bx) => {
                        drop_value_fast(v);
                        let (f, t, a) = *bx;
                        // A strict tail caller: hidden from `fn.caller` (see `tail_leave`).
                        let site = i.cur_site;
                        i.cur_site = site | crate::interpreter::frames::SITE_NATIVE;
                        let r = i.call(f, t, &a);
                        i.cur_site = site;
                        match r {
                            Ok(v) => v,
                            Err(Abrupt::Throw(e)) => {
                                pending_throw = Some(e);
                                continue;
                            }
                            Err(other) => {
                                while frames.live != 0 {
                                    leave_inline(i, &mut frames);
                                }
                                break Err(other);
                            }
                        }
                    }
                };
                match frames.top() {
                    Some(caller) => caller.stack.push(v),
                    None => stack.push(v),
                }
            }
            Ok(step) => {
                debug_assert!(frames.live == 0, "only the root frame can await");
                break Ok(step);
            }
            // Unwind to the innermost `try` handler, leaving inline frames that have none.
            Err(Abrupt::Throw(e)) => {
                let uncaught = loop {
                    match frames.top() {
                        Some(f) => match f.handlers.pop() {
                            Some(h) => {
                                f.stack.truncate(h.stack_depth);
                                f.stack.push(e);
                                f.pc = h.catch_pc;
                                break None;
                            }
                            None => leave_inline(i, &mut frames),
                        },
                        None => match handlers.pop() {
                            Some(h) => {
                                stack.truncate(h.stack_depth);
                                stack.push(e);
                                *pc = h.catch_pc;
                                break None;
                            }
                            None => break Some(e),
                        },
                    }
                };
                if let Some(e) = uncaught {
                    break Err(Abrupt::Throw(e));
                }
            }
            // Return/Break/Continue never escape a compiled body as an Abrupt; propagate defensively.
            Err(other) => {
                while frames.live != 0 {
                    leave_inline(i, &mut frames);
                }
                break Err(other);
            }
        }
    };
    if frames.recs.capacity() != 0 && i.vm_frame_pool.len() < 16 {
        i.vm_frame_pool.push(frames.recs);
    }
    r
}

/// The inline-frame stack of one [`drive_vm`]: `recs[..live]` are running frames (innermost
/// last); records past `live` are retired ones kept for reuse — they hold no values, only their
/// slot/operand/handler buffers' capacity, so a hot call site re-enters without allocating or
/// moving a record.
pub(crate) struct InlineFrames {
    recs: Vec<InlineFrame>,
    live: usize,
}

impl InlineFrames {
    #[inline]
    fn top(&mut self) -> Option<&mut InlineFrame> {
        match self.live {
            0 => None,
            n => Some(&mut self.recs[n - 1]),
        }
    }

    /// Make sure a retired record exists at `live` (may move the records: callers re-derive).
    #[inline]
    fn reserve_one(&mut self, i: &mut Interp) {
        if self.recs.len() == self.live {
            if self.recs.capacity() == 0 {
                if let Some(pooled) = i.vm_frame_pool.pop() {
                    self.recs = pooled;
                    if self.recs.len() > self.live {
                        return;
                    }
                }
            }
            self.recs.push(InlineFrame::default());
        }
    }
}

/// A compiled callee running inline on its caller's [`drive_vm`] loop: a bytecode→bytecode call
/// of an eligible function (see [`inline_callee`]) enters one of these instead of recursing
/// through `Interp::call` → `run`. It holds exactly what `run` would have kept on the Rust stack
/// (slot/operand buffers, handlers, pc, bound `this`, the run env) plus the engine flags the
/// ordinary call path saves and restores around the body ([`leave_inline`] undoes them).
///
/// The VM's working references point into the innermost record; they are re-derived after
/// every enter/leave (entering may grow the record vector and move the records — the buffers
/// they own never move).
#[derive(Default)]
pub(crate) struct InlineFrame {
    chunk: Option<Rc<Chunk>>,
    env: Option<Env>,
    this_val: Value,
    /// The callee function object, kept alive for the frame's `FnFrame` (`fn_ptr`) invariant.
    callee: Value,
    /// `[[Construct]]` frames: the fresh instance (the result unless the body returns an
    /// object). `Undefined` for calls.
    construct_this: Value,
    pc: usize,
    slots: Vec<Value>,
    stack: Vec<Value>,
    handlers: Vec<Handler>,
    saved_nt: Option<Value>,
    saved_strict: bool,
    saved_tco: bool,
    saved_field_init: bool,
    saved_agb: bool,
    saved_ctor: bool,
}

/// A callee [`drive_vm`] can run as an [`InlineFrame`]: an ordinary (non-class, non-proxy) user
/// function whose body is already compiled and is synchronous, in the active realm, not under a
/// `with`. Anything else — natives, bound functions, generators/async, not-yet-compiled bodies
/// (the ordinary path counts calls and tiers them up), the recursion limit — takes `Interp::call`.
pub(crate) struct InlineCallee {
    chunk: Rc<Chunk>,
    env: Env,
    strict: bool,
    arrow: bool,
    /// A base class constructor: its instance fields initialize on the new instance before the
    /// frame is entered (see [`class_fields::inline_class_ctor`]).
    class_fields: bool,
}

#[inline]
pub(crate) fn inline_callee(i: &Interp, callee: &Value) -> Option<InlineCallee> {
    let Value::Obj(o) = callee else {
        return None;
    };
    let b = o.borrow();
    let crate::value::Callable::User(u) = &b.call else {
        return None;
    };
    let f = &u.func;
    let Some(Some(chunk)) = f.code.get() else {
        return None;
    };
    if f.is_generator
        || f.is_async
        || i.depth >= i.depth_limit
        || matches!(i.tier, Tier::Interp)
        || i.multi_realm()
    {
        return None;
    }
    // Class constructors (never [[Call]]able) are strict non-arrow functions; a registered
    // proxy object is never inlined.
    let key = Gc::as_ptr(o) as usize;
    if (f.is_strict && !f.is_arrow && !i.class_info.is_empty() && i.class_info.contains_key(&key))
        || (!i.proxies.is_empty() && i.proxies.contains_key(&key))
        || u.env.borrow().under_with()
    {
        return None;
    }
    Some(InlineCallee {
        chunk: chunk.clone(),
        env: u.env.clone(),
        strict: f.is_strict,
        arrow: f.is_arrow,
        class_fields: false,
    })
}

/// [`inline_callee`] for `new`: an ordinary compiled constructor *function* (never a class —
/// class construction runs field initializers and `super` machinery on the ordinary path).
#[inline]
pub(crate) fn inline_ctor(i: &Interp, callee: &Value) -> Option<InlineCallee> {
    let Value::Obj(o) = callee else {
        return None;
    };
    let b = o.borrow();
    let crate::value::Callable::User(u) = &b.call else {
        return None;
    };
    let f = &u.func;
    let Some(Some(chunk)) = f.code.get() else {
        return None;
    };
    if f.is_arrow
        || f.is_method
        || f.is_generator
        || f.is_async
        || i.depth >= i.depth_limit
        || matches!(i.tier, Tier::Interp)
        || i.multi_realm()
    {
        return None;
    }
    let key = Gc::as_ptr(o) as usize;
    if (!i.class_info.is_empty() && i.class_info.contains_key(&key))
        || (!i.proxies.is_empty() && i.proxies.contains_key(&key))
        || u.env.borrow().under_with()
    {
        return None;
    }
    Some(InlineCallee {
        chunk: chunk.clone(),
        env: u.env.clone(),
        strict: f.is_strict,
        arrow: false,
        class_fields: false,
    })
}

/// The value the innermost inline frame's caller receives for its body's completion `v`: for a
/// `[[Construct]]` frame, `v` if it is an object, else the instance (after recording the
/// instance's size hint, as `construct_dispatch` does after a successful body).
#[inline]
fn construct_result(i: &mut Interp, frames: &mut InlineFrames, v: Value) -> Value {
    let f = &mut frames.recs[frames.live - 1];
    let Value::Obj(inst) = &f.construct_this else {
        return v;
    };
    if let Value::Obj(ctor) = &f.callee {
        i.observe_construct_capacity(ctor, inst);
    }
    match v {
        Value::Obj(_) => v,
        _ => std::mem::take(&mut f.construct_this),
    }
}

/// Enter an [`InlineFrame`] (in the retired record at `frames.live`, which
/// [`InlineFrames::reserve_one`] provided) for the call whose window sits on the caller's
/// `stack`: callee at `stack[at - 1]`, receiver below it when `has_this`, arguments at
/// `stack[at..]` (moved into the callee's slots). Counts the recursion depth and polls the
/// collector like `Interp::call`, then pops the call window off `stack`. On `Err` nothing was
/// changed.
///
/// SAFETY: `frames` is valid and has a retired record at `live`; `stack` is the caller's
/// operand stack (the root's, or record `live - 1`'s) — never the record being entered.
#[inline(never)]
unsafe fn enter_inline(
    i: &mut Interp,
    frames: *mut InlineFrames,
    stack: &mut Vec<Value>,
    at: usize,
    has_this: bool,
    construct: bool,
    c: InlineCallee,
) -> Result<(), Abrupt> {
    i.depth += 1;
    if let Err(e) = i.gc_check_amortized() {
        i.depth -= 1;
        return Err(e);
    }
    // [[Construct]] (construct_dispatch's user-function arm): OrdinaryCreateFromConstructor
    // with new.target = the callee itself.
    let instance = if construct {
        let own = match &stack[at - 1] {
            Value::Obj(o) => {
                i.materialize_fn(o);
                class_fields::own_prototype(o)
            }
            _ => None,
        };
        let proto = match own.map_or_else(|| i.get_member(&stack[at - 1], "prototype"), Ok) {
            Ok(Value::Obj(p)) => p,
            Ok(_) => i.object_proto.clone(),
            Err(e) => {
                i.depth -= 1;
                return Err(e);
            }
        };
        let Value::Obj(ctor) = &stack[at - 1] else {
            unreachable!("inline constructor is a function object")
        };
        let capacity = i.learned_construct_capacity(ctor);
        let inst = crate::value::Object::new_with_capacity(Some(proto), capacity);
        // A base class: InitializeInstanceElements precedes the body (and its parameters).
        if c.class_fields {
            if let Err(e) = i.init_instance_fields(&stack[at - 1], &Value::Obj(inst.clone())) {
                i.depth -= 1;
                return Err(e);
            }
        }
        Some(inst)
    } else {
        None
    };
    let f: &mut InlineFrame = &mut *(*frames).recs.as_mut_ptr().add((*frames).live);
    let (base, this) = if has_this {
        (at - 2, std::mem::take(&mut stack[at - 2]))
    } else {
        (at - 1, Value::Undefined)
    };
    let callee = std::mem::take(&mut stack[at - 1]);
    let argc = stack.len() - at;
    let seed = enter_frame(
        i,
        f,
        callee,
        this,
        instance,
        c,
        stack.as_mut_ptr().add(at),
        argc,
        true,
    );
    if seed == argc {
        // Everything above `base` was moved out (all `Undefined` now): nothing to drop.
        stack.set_len(base);
    } else {
        stack.truncate(base);
    }
    (*frames).live += 1;
    Ok(())
}

/// Fill retired record `f` for a call of `callee` (described by `c`): exactly the ordinary call
/// path's entry work (`call_dispatch` → `call_user` → `call_user_inner` → `run_compiled_chunk` →
/// [`run`]) in the same order. The `argc` arguments at `args` are moved into the parameter slots
/// when `move_args` (they are left `Undefined`), cloned otherwise. Returns how many were seeded.
///
/// SAFETY: `args..args + argc` is valid (and writable when `move_args`) and not inside `f`.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
unsafe fn enter_frame(
    i: &mut Interp,
    f: &mut InlineFrame,
    callee: Value,
    this: Value,
    instance: Option<Gc>,
    c: InlineCallee,
    args: *mut Value,
    argc: usize,
    move_args: bool,
) -> usize {
    let construct = instance.is_some();
    // A retired record's value fields are empty (see `leave_frame`): plain writes, no drops.
    let fn_ptr = match &callee {
        Value::Obj(o) => Gc::as_ptr(o) as usize,
        _ => unreachable!("inline callee is a function object"),
    };
    let this = match instance {
        // A construct leaves `constructing` alone and installs new.target = the callee (the
        // pending-new-target handoff of `construct_dispatch` → `call_user_inner`, net).
        Some(inst) => {
            f.saved_ctor = i.constructing;
            let nt = std::mem::replace(&mut i.new_target, callee.clone());
            std::ptr::write(&mut f.saved_nt, Some(nt));
            drop_value_fast(std::mem::take(&mut i.pending_new_target));
            drop_value_fast(this);
            std::ptr::write(&mut f.construct_this, Value::Obj(inst.clone()));
            Value::Obj(inst)
        }
        None => {
            f.saved_ctor = std::mem::replace(&mut i.constructing, false);
            // An ordinary call clears new.target (an arrow inherits it). When it is already
            // undefined — nearly always — there is nothing to save or restore: every callee puts
            // back what it changed. (Skipping the write also avoids a partial-width store that
            // the next call's full-width read would stall on.)
            if !c.arrow && !matches!(i.new_target, Value::Undefined) {
                std::ptr::write(
                    &mut f.saved_nt,
                    Some(std::mem::replace(&mut i.new_target, Value::Undefined)),
                );
            }
            this
        }
    };
    std::ptr::write(&mut f.callee, callee);
    jit::sync_frames(i);
    i.fn_frames.push(crate::interpreter::FnFrame {
        fn_ptr,
        coro: i.cur_coro,
        caller_site: std::mem::replace(&mut i.cur_site, crate::interpreter::frames::NO_SITE),
        strict: c.strict,
        construct,
        extra: None,
    });
    let chunk = &*c.chunk;
    let this_val = if chunk.uses_this() {
        i.bind_compiled_this_flags(c.strict, chunk, this, construct)
    } else {
        drop_value_fast(this);
        Value::Undefined
    };
    std::ptr::write(&mut f.this_val, this_val);
    f.saved_strict = std::mem::replace(&mut i.strict, c.strict);
    f.saved_tco = std::mem::replace(&mut i.tco_ok, c.strict && !construct);
    f.saved_field_init = i.in_field_init_code;
    f.saved_agb = i.in_async_gen_body;
    if !c.arrow {
        i.in_field_init_code = false;
        i.in_async_gen_body = false;
    }
    let arg_slice = std::slice::from_raw_parts_mut(args, argc);
    let env = match &chunk.activation_layout {
        Some(layout) => layout.make_env(chunk, i, &c.env, &f.this_val, arg_slice),
        None => c.env,
    };
    let args_obj = match chunk.arguments_slot {
        Some(_) if chunk.virt_base.is_none() => {
            Some(i.make_compiled_arguments_object(arg_slice, &env))
        }
        _ => None,
    };
    let slots = &mut f.slots;
    let seed = chunk.n_params.min(argc);
    slots.reserve(chunk.n_slots);
    if move_args {
        // No refcount traffic: the caller's window is discarded right after.
        for v in &mut arg_slice[..seed] {
            slots.push(std::mem::take(v));
        }
    } else {
        slots.extend_from_slice(&arg_slice[..seed]);
    }
    while slots.len() < chunk.n_slots {
        slots.push(Value::Undefined);
    }
    if let (Some(s), Some(ao)) = (chunk.arguments_slot, args_obj) {
        slots[s as usize] = Value::Obj(ao);
    }
    if chunk.reflect_args && reflect::enabled() {
        // The leading arguments may have been moved into the slots (still unmodified).
        reflect::stash_compiled(i, &slots[..seed], &arg_slice[seed..], slots.as_ptr());
    }
    chunk.seed_rest(i, slots, arg_slice);
    for &s in &chunk.var_force_resets {
        slots[s as usize] = Value::Undefined;
    }
    std::ptr::write(&mut f.env, Some(env));
    std::ptr::write(&mut f.chunk, Some(c.chunk));
    f.pc = 0;
    seed
}

/// Tail calls that cannot release their frame in place — made by native code's generic
/// call-out (`Op::TailCall` with `ONE`), or from a root frame to a callee that cannot run as an
/// inline frame — run as ordinary calls up to this recursion depth, and through the caller's
/// trampoline (`Interp::pending_tail`) beyond it: bounded native stack either way. Fixed rather
/// than a share of `MAX_EVAL_DEPTH`: tail recursion must fit any thread's stack (a 2 MB test
/// thread included), whatever the ceiling for ordinary recursion.
pub(crate) const TAIL_NEST: u32 = 96;

/// Native stack a tail call made as an ordinary call must leave free: past it the call takes the
/// trampoline even within `TAIL_NEST` (unoptimized builds spend ~100 KiB per nested call).
const TAIL_ROOM: usize = 512 * 1024;

/// Whether a tail call that cannot release its frame in place goes through the trampoline.
#[inline]
pub(crate) fn tail_nest_exceeded(i: &Interp) -> bool {
    i.depth > TAIL_NEST || crate::stack::headroom().is_none_or(|r| r < TAIL_ROOM)
}

/// PrepareForTailCall in the innermost [`InlineFrame`]: move its top `n` operand-stack entries
/// (the call window) onto its caller's operand stack (the root's `root_stack` when it is the
/// only live frame), then leave it.
///
/// SAFETY: `frames.live != 0`, the innermost record's stack is synced and holds `n` entries,
/// and `root_stack` is the root frame's (synced) operand stack.
#[inline(always)]
unsafe fn tail_leave(
    i: &mut Interp,
    frames: &mut InlineFrames,
    root_stack: *mut Vec<Value>,
    n: usize,
) {
    let live = frames.live;
    let recs = frames.recs.as_mut_ptr();
    let cur = &mut (*recs.add(live - 1)).stack;
    let caller: &mut Vec<Value> = if live >= 2 {
        &mut (*recs.add(live - 2)).stack
    } else {
        &mut *root_stack
    };
    let from = cur.len() - n;
    caller.reserve(n);
    let len = caller.len();
    std::ptr::copy_nonoverlapping(cur.as_ptr().add(from), caller.as_mut_ptr().add(len), n);
    caller.set_len(len + n);
    cur.set_len(from);
    leave_inline(i, frames);
    // The callee's caller (this strict frame) is gone: `fn.caller` reports null for it.
    i.cur_site |= crate::interpreter::frames::SITE_NATIVE;
}

/// The call window on top of `stack` (callee, receiver when `has_this`, `argc` arguments),
/// taken off as an `Interp::pending_tail` entry.
fn tail_window(
    stack: &mut Vec<Value>,
    argc: usize,
    has_this: bool,
) -> Box<(Value, Value, Vec<Value>)> {
    let args = stack.split_off(stack.len() - argc);
    let callee = stack.pop().expect("tail call callee");
    let this = if has_this {
        stack.pop().expect("tail call receiver")
    } else {
        Value::Undefined
    };
    Box::new((callee, this, args))
}

/// A call op's error `e` (the op at `pc` of `chunk`, whose callee was `callee`): when the callee
/// is not callable — so `e` is `Interp::call`'s generic "<type> is not a function" — the
/// TypeError the tree-walker throws instead, naming the callee expression as written.
#[cold]
#[inline(never)]
fn call_error(i: &mut Interp, chunk: &Chunk, pc: usize, callee: &Value, e: Abrupt) -> Abrupt {
    if callee.is_callable() {
        return e;
    }
    jit::sync_frames(i);
    let desc = i.innermost_source().and_then(|src| {
        crate::interpreter::stack_trace::describe_callee_text(&src, chunk.call_site_pos(pc))
    });
    match desc {
        Some(d) => i.throw("TypeError", format!("{d} is not a function")),
        None => e,
    }
}

/// [`call_error`] for native code's generic call helper: the call op is the one
/// `Interp::cur_site` names in `chunk`.
#[cold]
pub(crate) fn site_call_error(i: &mut Interp, chunk: &Chunk, callee: &Value, e: Abrupt) -> Abrupt {
    use crate::interpreter::frames::{site_pos, NO_SITE, SITE_PC};
    let site = site_pos(i.cur_site);
    if site == NO_SITE || site & SITE_PC == 0 {
        return e;
    }
    call_error(i, chunk, (site & !SITE_PC) as usize, callee, e)
}

/// Leave the innermost [`InlineFrame`] (returned or unwound) and give back its recursion depth.
fn leave_inline(i: &mut Interp, frames: &mut InlineFrames) {
    frames.live -= 1;
    leave_frame(i, &mut frames.recs[frames.live]);
    i.depth -= 1;
}

/// The exit half of the ordinary call path, in the same order — buffers emptied, per-body flags,
/// the reflection frame, the dispatch's `constructing`/`new.target`. The record is retired in
/// place (every value field emptied).
fn leave_frame(i: &mut Interp, f: &mut InlineFrame) {
    clear_values_fast(&mut f.slots);
    clear_values_fast(&mut f.stack);
    f.handlers.clear();
    drop_value_fast(std::mem::take(&mut f.this_val));
    f.env = None;
    f.chunk = None;
    i.strict = f.saved_strict;
    i.tco_ok = f.saved_tco;
    i.in_field_init_code = f.saved_field_init;
    i.in_async_gen_body = f.saved_agb;
    // Pop the reflection frame reading only its `extra` word (the frame was written field by
    // field; a whole-record read would stall on store forwarding).
    let n = i.fn_frames.len() - 1;
    i.cur_site = i.fn_frames[n].caller_site;
    if i.fn_frames[n].extra.is_some() {
        i.fn_frames.truncate(n);
    } else {
        // SAFETY: the remaining fields of `FnFrame` are plain data.
        unsafe { i.fn_frames.set_len(n) };
    }
    i.constructing = f.saved_ctor;
    if let Some(nt) = f.saved_nt.take() {
        drop_value_fast(std::mem::replace(&mut i.new_target, nt));
    }
    drop_value_fast(std::mem::take(&mut f.callee));
    drop_value_fast(std::mem::take(&mut f.construct_this));
}

/// `Interp::call`'s shortcut for a callee [`inline_callee`] accepted: run the compiled body on a
/// fresh [`drive_vm`] (its own calls then run inline there) without the generic dispatch chain.
/// The caller has already counted the recursion depth and polled the collector.
pub(crate) fn call_compiled(
    i: &mut Interp,
    callee: Value,
    this: Value,
    args: &[Value],
    c: InlineCallee,
) -> Result<Value, Abrupt> {
    let mut rec = i.vm_frame_one.pop().unwrap_or_default();
    // SAFETY: `args` is only read (not moved from) with `move_args == false`.
    unsafe {
        enter_frame(
            i,
            &mut rec,
            callee,
            this,
            None,
            c,
            args.as_ptr() as *mut Value,
            args.len(),
            false,
        )
    };
    let r = {
        let InlineFrame {
            chunk,
            env,
            this_val,
            pc,
            slots,
            stack,
            handlers,
            ..
        } = &mut *rec;
        // SAFETY: `enter_frame` filled both.
        let (chunk, env) = unsafe {
            (
                chunk.as_deref().unwrap_unchecked(),
                env.as_ref().unwrap_unchecked(),
            )
        };
        drive_vm(i, chunk, env, slots, stack, pc, this_val, handlers, None)
    };
    leave_frame(i, &mut rec);
    if i.vm_frame_one.len() < 64 {
        i.vm_frame_one.push(rec);
    }
    match r? {
        VmStep::Done(v) => Ok(v),
        VmStep::Await(_) => unreachable!("a synchronous bytecode function cannot await"),
    }
}

/// `Op::MakeClosure(fidx, name_n)`: the closure of `chunk.funcs[fidx]` over `env`, named
/// `chunk.names[name_n]` by NamedEvaluation unless `name_n` is `u32::MAX`. The whole op, for
/// the interpreter loop and for native code's call-out (it reads no slot and moves no pc).
#[inline]
pub(crate) fn make_closure(
    i: &mut Interp,
    chunk: &Chunk,
    fidx: u32,
    name_n: u32,
    env: &Env,
) -> Value {
    let func = chunk.funcs[fidx as usize].clone();
    if name_n == u32::MAX {
        return i.make_function(func, env.clone());
    }
    let name = &chunk.names[name_n as usize];
    let (v, named) = i.make_function_named(func, env.clone(), Some(name));
    if !named {
        i.set_fn_name(&v, name);
    }
    v
}

/// Drop a `Value`, skipping the out-of-line drop glue for the refcount-free variants (tags
/// `0..=4`, see [`Value`]'s layout note) — most slot and operand values on a hot call path.
#[inline(always)]
fn drop_value_fast(v: Value) {
    if matches!(
        v,
        Value::Undefined | Value::Empty | Value::Null | Value::Bool(_) | Value::Num(_)
    ) {
        std::mem::forget(v);
    } else {
        drop(v);
    }
}

/// `values.clear()` through [`drop_value_fast`].
#[inline(always)]
fn clear_values_fast(values: &mut Vec<Value>) {
    while let Some(v) = values.pop() {
        drop_value_fast(v);
    }
}

/// Run from `*pc` until the body returns (`Done`), suspends at an `await` (`Await`, async bodies
/// only), or throws (`Err(Abrupt::Throw)`, caught by [`drive_vm`]). Operates on borrowed state so an
/// async [`VmCoro`] can save it at a suspension and restore it on resume.
///
/// `ONE = true` executes exactly the op at `*pc` and returns `Ok(VmStep::Done(Value::Empty))`
/// (the optimizing tier's generic fallback, `jit::helpers::generic`, which never feeds it an op
/// that returns or awaits). The check sits at the loop head so arms that `continue` stop too, and
/// it compiles away in the interpreter's `ONE = false` instantiation.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn run_vm<const ONE: bool>(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    handlers: &mut Vec<Handler>,
) -> Result<VmStep, Abrupt> {
    run_vm_frames::<ONE>(
        i,
        chunk,
        env,
        slots,
        stack,
        pc,
        this_val,
        handlers,
        std::ptr::null_mut(),
    )
}

#[cfg(not(feature = "compiler"))]
#[allow(clippy::too_many_arguments)]
fn run_vm_frames<const ONE: bool>(
    i: &mut Interp,
    _chunk: &Chunk,
    _env: &Env,
    _slots: &mut [Value],
    _stack: &mut Vec<Value>,
    _pc: &mut usize,
    _this: &Value,
    _handlers: &mut Vec<Handler>,
    _frames: *mut InlineFrames,
) -> Result<VmStep, Abrupt> {
    Err(i.throw(
        "EvalError",
        "bytecode execution is not available in an AOT build",
    ))
}

/// [`run_vm`] plus inline frames: with a non-null `frames` (only [`drive_vm`] passes one), a
/// call to an eligible compiled function pushes an [`InlineFrame`] and keeps executing in this
/// loop on the callee's state; the body's `Done`/throw then surfaces to `drive_vm`, which pops
/// or unwinds the frame and re-enters here on the caller's state. Execution always resumes in
/// the innermost frame, so the working state below is derived from `frames.last()` (the root
/// arguments otherwise).
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "compiler")]
fn run_vm_frames<const ONE: bool>(
    i: &mut Interp,
    root_chunk: &Chunk,
    root_env: &Env,
    root_slots: &mut [Value],
    root_stack: &mut Vec<Value>,
    root_pc: &mut usize,
    root_this: &Value,
    root_handlers: &mut Vec<Handler>,
    frames: *mut InlineFrames,
) -> Result<VmStep, Abrupt> {
    // SAFETY: the root references are exclusively ours for this call; inline-frame references
    // point into `frames` records (or the buffers they own) and are re-derived by `load_frame!`
    // after every push, before any of the previous references is used again. Nothing else
    // touches `frames` while this loop runs (reentrant calls get their own `drive_vm`).
    let root_slots: *mut [Value] = root_slots;
    let root_stack: *mut Vec<Value> = root_stack;
    let root_pc: *mut usize = root_pc;
    let root_handlers: *mut Vec<Handler> = root_handlers;
    let mut chunk: &Chunk;
    let mut env: &Env;
    let mut slots: &mut [Value];
    // The operand stack as raw pointers into the frame's `Vec` (see [`VmStack`]).
    let mut stack: VmStack = unsafe { VmStack::new(root_stack) };
    // The working pc lives in a register-promotable local (see [`PcReg`]), written back to the
    // frame's slot on every exit and frame switch — not through a pointer on every op.
    let mut pc = PcReg {
        v: unsafe { *root_pc },
        dst: root_pc,
    };
    let mut ops: &[Op];
    let mut this_val: &Value;
    let mut handlers: &mut Vec<Handler>;
    macro_rules! load_frame {
        () => {
            unsafe { *pc.dst = pc.v };
            match if ONE || frames.is_null() {
                None
            } else {
                unsafe { (*frames).top().map(|f| f as *mut InlineFrame) }
            } {
                Some(f) => unsafe {
                    chunk = (*f).chunk.as_deref().unwrap_unchecked();
                    env = (*f).env.as_ref().unwrap_unchecked();
                    slots = (&mut (*f).slots).as_mut_slice();
                    stack.attach(&mut (*f).stack);
                    pc.dst = &mut (*f).pc;
                    pc.v = (*f).pc;
                    this_val = &(*f).this_val;
                    handlers = &mut (*f).handlers;
                },
                None => unsafe {
                    chunk = root_chunk;
                    env = root_env;
                    slots = &mut *root_slots;
                    stack.attach(root_stack);
                    pc.dst = root_pc;
                    pc.v = *root_pc;
                    this_val = root_this;
                    handlers = &mut *root_handlers;
                },
            }
            ops = &chunk.ops;
        };
    }
    load_frame!();
    // --- whole-function JIT tier: a freshly pushed inline frame (pc 0) may run native function
    // code. Its completion returns as the frame's `Done` (the driver hands it to the caller); a
    // throw unwinds from the frame like any op's.
    macro_rules! jit_entry {
        () => {
            if jit::entry_due(i, chunk) {
                let r = jit::on_entry(i, chunk, env, slots, stack.vec(), &mut pc.v, this_val);
                unsafe { stack.reload() };
                unsafe { *pc.dst = pc.v };
                if let Some(step) = r? {
                    return Ok(step);
                }
            }
        };
    }
    // --- end whole-function JIT tier ---
    // Push a prepared inline frame and switch the working state to it.
    // Call the function at `stack[at - 1]` as an inline frame if it qualifies (see
    // [`inline_callee`]) and switch the working state to it; otherwise fall through to the
    // ordinary call.
    macro_rules! try_inline_call {
        ($at:expr, $has_this:expr) => {
            if !ONE && !frames.is_null() {
                if let Some(c) = inline_callee(i, &stack[$at - 1]) {
                    // Reserving may move the records (and with them the caller's `stack` and
                    // `pc` homes; the stack's buffer stays put).
                    stack.sync();
                    unsafe { (*frames).reserve_one(i) };
                    match unsafe { (*frames).top() } {
                        Some(f) => {
                            stack.repoint(&mut f.stack);
                            pc.dst = &mut f.pc;
                        }
                        None => {
                            stack.repoint(root_stack);
                            pc.dst = root_pc;
                        }
                    }
                    // Changes the caller's `Vec` (the call window is moved out): the view is
                    // stale until `load_frame!` switches to the callee.
                    unsafe { enter_inline(i, frames, stack.vec(), $at, $has_this, false, c)? };
                    load_frame!();
                    jit_entry!();
                    continue;
                }
            }
        };
    }
    macro_rules! pop {
        () => {
            stack.pop_val()
        };
    }
    // An inline frame's body completed with `$v`: hand it to the caller and keep running there
    // without leaving this loop — what `drive_vm` does for a `Done` from an inline frame
    // (`construct_result`, `leave_inline`, push onto the caller's stack).
    macro_rules! return_inline {
        ($v:expr) => {
            if !ONE && !frames.is_null() && unsafe { (*frames).live } != 0 {
                // `leave_inline` clears the callee's stack through its `Vec`.
                stack.sync();
                let v = unsafe { construct_result(i, &mut *frames, $v) };
                unsafe { leave_inline(i, &mut *frames) };
                load_frame!();
                stack.push(v);
                continue;
            }
        };
    }
    // `Op::TailCall` / `Op::TailCallSpread` with the call window on the stack (callee at
    // `stack[len - $argc - 1]`, receiver below it when `$has_this`, `$argc` arguments on top);
    // the op's `Return` follows. PrepareForTailCall when this frame may tail-call (strict, not
    // `[[Construct]]`) and the callee is callable (a TypeError is this frame's): see
    // [`self_tail`]. Otherwise, or where no frame can be released, an ordinary call.
    macro_rules! tail_call {
        ($argc:expr, $has_this:expr) => {{
            let argc: usize = $argc;
            let has_this: bool = $has_this;
            let at = stack.len() - argc;
            if !ONE && !frames.is_null() && i.tco_ok && stack[at - 1].is_callable() {
                if unsafe { (*frames).live } != 0 {
                    // An inline frame: the window moves to its caller's stack, the frame is
                    // left, and the call is made from the caller's call site — inline again when
                    // the callee qualifies, reusing the record: constant space.
                    stack.sync();
                    unsafe {
                        tail_leave(i, &mut *frames, root_stack, argc + 1 + has_this as usize)
                    };
                    load_frame!();
                    let at = stack.len() - argc;
                    try_inline_call!(at, has_this);
                    let callee = stack[at - 1].clone();
                    let this = if has_this {
                        stack[at - 2].clone()
                    } else {
                        Value::Undefined
                    };
                    let v = i.call(callee, this, &stack[at..])?;
                    stack.truncate(at - 1 - has_this as usize);
                    stack.push(v);
                    continue;
                }
                // The root frame: an inline callee runs as the first inline frame (its own tail
                // calls then replace it) while this frame waits in its `Return`.
                try_inline_call!(at, has_this);
                if tail_nest_exceeded(i) && i.leaf_native(&stack[at - 1]).is_none() {
                    // Past `TAIL_NEST`, anything else but a plain native (which cannot come back
                    // through this frame): the `Interp::call` trampoline (or the compiled-call
                    // paths that mirror it) makes the call once this body has returned, so tail
                    // recursion through such callees stays within `TAIL_NEST` native frames.
                    // (Shallower, an ordinary call saves the hand-off.)
                    i.pending_tail = Some(tail_window(stack.vec(), argc, has_this));
                    unsafe { stack.reload() };
                    return Ok(VmStep::Done(Value::Undefined));
                }
            } else if ONE && i.tco_ok && tail_nest_exceeded(i) && stack[at - 1].is_callable() {
                // Native code's generic call-out (no frame to release here): an ordinary call
                // while shallow; past `TAIL_NEST` the call is handed to the caller's trampoline
                // (the pushed `undefined` is what the following `Return` returns), so unbounded
                // tail recursion through compiled code stays within `TAIL_NEST` frames.
                i.pending_tail = Some(tail_window(stack.vec(), argc, has_this));
                unsafe { stack.reload() };
                stack.push(Value::Undefined);
                continue;
            }
            if !ONE && !i.tco_ok {
                try_inline_call!(at, has_this);
            }
            let callee = stack[at - 1].clone();
            let this = if has_this {
                stack[at - 2].clone()
            } else {
                Value::Undefined
            };
            let v = i
                .call(callee, this, &stack[at..])
                .map_err(|e| call_error(i, chunk, *pc - 1, &stack[at - 1], e))?;
            stack.truncate(at - 1 - has_this as usize);
            stack.push(v);
            continue;
        }};
    }
    let mut stepped = false;
    loop {
        if ONE {
            if stepped {
                return Ok(VmStep::Done(Value::Empty));
            }
            stepped = true;
        }
        let op = ops[*pc];
        if matches!(
            op,
            Op::Add
                | Op::Sub
                | Op::Mul
                | Op::Div
                | Op::Mod
                | Op::Lt
                | Op::Le
                | Op::Gt
                | Op::Ge
                | Op::EqEq
                | Op::NotEq
                | Op::StrictEq
                | Op::StrictNotEq
        ) {
            let length = stack.len();
            if length >= 2 {
                crate::feedback::observe(
                    || chunk.feedback_key(),
                    *pc,
                    &stack[length - 2],
                    &stack[length - 1],
                );
            }
        } else if matches!(op, Op::GetProp(..) | Op::GetMethod(..)) {
            if let Some(value) = stack.last() {
                crate::feedback::observe(|| chunk.feedback_key(), *pc, value, &Value::Undefined);
            }
        }
        *pc += 1;
        #[cfg(lumen_op_stats)]
        vm_regs::op_stat(&op);
        match op {
            Op::Const(k) => stack.push(vm_regs::clone_fast(&chunk.consts[k as usize])),
            Op::Undef => stack.push_tag(0),
            Op::Dup => {
                let t = vm_regs::clone_fast(stack.last().expect("vm stack underflow"));
                stack.push(t);
            }
            Op::Pop => stack.discard(),
            Op::LoadLocal(s) => {
                let v = vm_regs::clone_fast(&slots[s as usize]);
                if matches!(v, Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                stack.push(v);
            }
            Op::StoreLocal(s) => {
                let v = pop!();
                vm_regs::set_slot(&mut slots[s as usize], v);
            }
            Op::UpdateLocal(s, kind) => {
                let idx = s as usize;
                match &slots[idx] {
                    // Reading a slot still in its TDZ is the same ReferenceError as LoadLocal.
                    Value::Empty => {
                        return Err(i.throw(
                            "ReferenceError",
                            format!(
                                "cannot access '{}' before initialization",
                                chunk.slot_names[idx]
                            ),
                        ));
                    }
                    // Fast path: a numeric slot updates in place.
                    Value::Num(n) => {
                        let old = *n;
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => old + 1.0,
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => old - 1.0,
                        };
                        // The slot holds a Num: overwrite without running its drop glue.
                        unsafe { vm_regs::write_num(&mut slots[idx], new) };
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push_num(new),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push_num(old),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                    // BigInt updates stay BigInt (ToNumeric, not ToNumber) — never coerced to a
                    // Number and never thrown on like unary `+` would.
                    Value::BigInt(n) => {
                        let old = n.clone();
                        let inc = matches!(
                            kind,
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard
                        );
                        let new = match old.checked_step(inc) {
                            Ok(v) => v,
                            Err(e) => return Err(i.throw("RangeError", e.message())),
                        };
                        slots[idx] = Value::BigInt(new.clone());
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::BigInt(new)),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::BigInt(old)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                    // Anything else: ToNumber (may run user `valueOf`), then a Number update. The
                    // post value is the *coerced* number, matching the tree-walker's `eval_update`.
                    _ => {
                        let old = slots[idx].clone();
                        let coerced = i.to_number(&old)?;
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => {
                                coerced + 1.0
                            }
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => {
                                coerced - 1.0
                            }
                        };
                        slots[idx] = Value::Num(new);
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::Num(new)),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::Num(coerced)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                }
            }
            Op::Tdz(s) => {
                let slot = &mut slots[s as usize];
                vm_regs::set_slot(slot, Value::Empty);
                unsafe { vm_regs::write_tag(slot, 1) };
            }
            Op::LoadCap(n) => {
                stack.push(chunk.load_cap_ic(i, env, n)?);
            }
            Op::StoreCap(n) => {
                let v = pop!();
                chunk.store_cap_ic(i, env, n, v, false)?;
            }
            Op::StoreCapInit(n) => {
                let v = pop!();
                chunk.store_cap_ic(i, env, n, v, true)?;
            }
            Op::UpdateCap(n, kind) => {
                let name = &chunk.names[n as usize];
                let old = {
                    let b = env.borrow();
                    let bd = b.vars.get(&**name).expect("captured binding missing");
                    if !bd.initialized {
                        let msg = format!("cannot access '{name}' before initialization");
                        drop(b);
                        return Err(i.throw("ReferenceError", msg));
                    }
                    bd.value.clone()
                };
                step_and_store(i, &mut stack, kind, old, |_, v| {
                    if let Some(bd) = env.borrow_mut().vars.get_mut(name) {
                        bd.value = v;
                    }
                    Ok(())
                })?;
            }
            Op::UpdateName(n, kind) => {
                let name = &chunk.names[n as usize];
                let old = i.get_var(name, env)?;
                step_and_store(i, &mut stack, kind, old, |i, v| {
                    i.assign_free_name(name, v, env)
                })?;
            }
            Op::UpdateNameCached(n, c, kind) => {
                let old = chunk.load_name_ic(i, env, n, c)?;
                step_and_store(i, &mut stack, kind, old, |i, v| {
                    chunk.store_name_ic(i, env, n, c, v)
                })?;
            }
            Op::MakeClosure(fidx, name_n) => {
                stack.push(make_closure(i, chunk, fidx, name_n, env));
            }
            Op::LoadName(n, c) => {
                let v = chunk.load_name_ic(i, env, n, c)?;
                stack.push(v);
            }
            Op::StoreName(n) => {
                let v = pop!();
                i.assign_free_name(&chunk.names[n as usize], v, env)?;
            }
            Op::StoreNameCached(n, c) => {
                let v = pop!();
                chunk.store_name_ic(i, env, n, c, v)?;
            }
            Op::LoadThis => stack.push(this_val.clone()),
            Op::LoadLexicalThis => stack.push(i.lexical_this(env)?),
            Op::GetProp(n, c) => {
                let obj = pop!();
                let v = i.get_prop_ic(&obj, &chunk.names[n as usize], &chunk.caches[c as usize])?;
                stack.push(v);
            }
            Op::GetPropThis(n, c) => {
                let v = i.get_prop_ic(
                    this_val,
                    &chunk.names[n as usize],
                    &chunk.caches[c as usize],
                )?;
                stack.push(v);
            }
            Op::GetPropLocal(s, n, c) => {
                let obj = &slots[s as usize];
                if matches!(obj, Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                // Borrowed, not cloned: only this frame's own code writes its slots, and none
                // runs during the read (like GetPropThis's receiver).
                let v = i.get_prop_ic(obj, &chunk.names[n as usize], &chunk.caches[c as usize])?;
                stack.push(v);
            }
            Op::SetProp(n, c) => {
                let v = pop!();
                let obj = pop!();
                i.set_prop_ic(
                    &obj,
                    &chunk.names[n as usize],
                    v.clone(),
                    &chunk.caches[c as usize],
                )?;
                stack.push(v);
            }
            Op::SetPropDrop(n, c) => {
                let v = pop!();
                let obj = pop!();
                i.set_prop_ic(&obj, &chunk.names[n as usize], v, &chunk.caches[c as usize])?;
            }
            Op::SetPropThisDrop(n, c) => {
                let v = pop!();
                i.set_prop_ic(
                    this_val,
                    &chunk.names[n as usize],
                    v,
                    &chunk.caches[c as usize],
                )?;
            }
            Op::SetPropLocalDrop(s, n, c) => {
                let v = pop!();
                let obj = slots[s as usize].clone();
                if matches!(obj, Value::Empty) {
                    return Err(i.throw(
                        "ReferenceError",
                        format!(
                            "cannot access '{}' before initialization",
                            chunk.slot_names[s as usize]
                        ),
                    ));
                }
                i.set_prop_ic(&obj, &chunk.names[n as usize], v, &chunk.caches[c as usize])?;
            }
            Op::DestructureGuard => {
                crate::native_ops::destructure_guard(i, stack.last().expect("vm stack underflow"))?;
            }
            Op::DestructureArr(n) => {
                let v = pop!();
                if !array_destructure::try_dense(i, &v, n, |x| stack.push(x)) {
                    // A pristine Array/Map/Set walks an encoded state (see `iter_fast`).
                    let (mut it, mut nx) = match iter_fast::open(i, &v) {
                        Some(state) => (Value::Num(state), v.clone()),
                        None => i.get_iterator(&v)?,
                    };
                    let mut done = false;
                    for _ in 0..n {
                        if !done {
                            let stepped = match iter_fast::step(i, &mut it, &mut nx) {
                                Some(r) => r,
                                None => i.iterator_step(&it, &nx)?,
                            };
                            match stepped {
                                Some(x) => {
                                    stack.push(x);
                                    continue;
                                }
                                None => done = true,
                            }
                        }
                        stack.push(Value::Undefined);
                    }
                    if !done && !iter_fast::close_pair_is_noop(i, &mut it, &mut nx) {
                        i.iterator_close_normal(&it)?;
                    }
                }
            }
            Op::DeleteProp(n, strict) => {
                let base = pop!();
                let v =
                    crate::native_ops::delete_property(i, base, &chunk.names[n as usize], strict)?;
                stack.push(v);
            }
            Op::DeleteElem(strict) => {
                let idx = pop!();
                let base = pop!();
                let v = crate::native_ops::delete_element(i, base, idx, strict)?;
                stack.push(v);
            }
            Op::CallSpread(argc) | Op::CallSpreadThis(argc) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                let spread = pop!();
                let at = stack.len() - (argc as usize - 1);
                // A pristine Array spreads as a slice copy (see `iter_fast`): the window is then
                // an ordinary `Call` / `CallWithThis` one, inlined like theirs.
                if iter_fast::array_elems(i, &spread, |x| stack.push(x)) {
                    drop(spread);
                    let has_this = matches!(op, Op::CallSpreadThis(_));
                    try_inline_call!(at, has_this);
                    let callee = stack[at - 1].clone();
                    let this = if has_this {
                        stack[at - 2].clone()
                    } else {
                        Value::Undefined
                    };
                    let v = i
                        .call(callee, this, &stack[at..])
                        .map_err(|e| call_error(i, chunk, *pc - 1, &stack[at - 1], e))?;
                    stack.truncate(at - 1 - has_this as usize);
                    stack.push(v);
                    continue;
                }
                let mut args: Vec<Value> = stack.split_off(at);
                let (it, nx) = i.get_iterator(&spread)?;
                while let Some(x) = i.iterator_step(&it, &nx)? {
                    args.push(x);
                }
                let callee = pop!();
                let this = if matches!(op, Op::CallSpreadThis(_)) {
                    pop!()
                } else {
                    Value::Undefined
                };
                let v = i
                    .call(callee.clone(), this, &args)
                    .map_err(|e| call_error(i, chunk, *pc - 1, &callee, e))?;
                stack.push(v);
            }
            Op::AppendProp(n, c) => {
                let v = pop!();
                let lval = pop!();
                let obj = pop!();
                let name = &chunk.names[n as usize];
                let lval = if let (Value::Str(x), Value::Obj(o)) = (&v, &obj) {
                    match i.append_prop_fast(o, name, lval, x) {
                        Ok(()) => continue,
                        Err(l) => l,
                    }
                } else {
                    lval
                };
                let r = i.binary("+", lval, v)?;
                i.set_prop_ic(&obj, name, r, &chunk.caches[c as usize])?;
            }
            Op::GetElem => {
                let key = pop!();
                let obj = pop!();
                match (&obj, &key) {
                    (Value::Obj(o), Value::Num(n)) => {
                        if let Some(v) = i.fast_get_elem(o, *n) {
                            stack.push(v);
                            continue;
                        }
                    }
                    (Value::Str(s), Value::Num(n)) => {
                        if let Some(v) = str_index_fast(s, *n).or_else(|| str_index_units(i, s, *n))
                        {
                            stack.push(v);
                            continue;
                        }
                    }
                    (Value::Obj(o), Value::Str(k)) => {
                        if let Some(v) = i.fast_get_str(o, k) {
                            stack.push(v);
                            continue;
                        }
                    }
                    _ => {}
                }
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", i.null_read_message(&obj, Some(&key))));
                }
                let k = i.to_property_key(&key)?;
                let v = i.get_member(&obj, &k)?;
                stack.push(v);
            }
            Op::SetElem => {
                let v = pop!();
                let key = pop!();
                let obj = pop!();
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    let ret = v.clone();
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => {
                            stack.push(ret);
                            continue;
                        }
                        Err(back) => {
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            stack.push(ret);
                            continue;
                        }
                    }
                }
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v.clone())?;
                stack.push(v);
            }
            Op::SetElemDrop => {
                let v = pop!();
                let key = pop!();
                let obj = pop!();
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => continue,
                        Err(back) => {
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            continue;
                        }
                    }
                }
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v)?;
            }
            Op::ArgsLen(s, tag) => {
                let v = virt_len(i, slots, s, tag)?;
                stack.push(v);
            }
            Op::ArgsGet(s, tag) => {
                let key = pop!();
                let v = virt_get(i, slots, s, tag, key)?;
                stack.push(v);
            }
            Op::ApplyArgs(s, tag) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                let at = stack.len() - 3;
                let v = virt_apply(i, slots, s, tag, &stack[at..])
                    .map_err(|e| call_error(i, chunk, *pc - 1, &stack[at + 1], e))?;
                stack.truncate(at);
                stack.push(v);
            }
            Op::GetElemLocal(s) => {
                let key = pop!();
                match (&slots[s as usize], &key) {
                    (Value::Obj(o), Value::Num(n)) => {
                        if let Some(v) = i.fast_get_elem(o, *n) {
                            stack.push(v);
                            continue;
                        }
                    }
                    (Value::Str(st), Value::Num(n)) => {
                        if let Some(v) =
                            str_index_fast(st, *n).or_else(|| str_index_units(i, st, *n))
                        {
                            stack.push(v);
                            continue;
                        }
                    }
                    _ => {}
                }
                let obj = slots[s as usize].clone();
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", i.null_read_message(&obj, Some(&key))));
                }
                let k = i.to_property_key(&key)?;
                let v = i.get_member(&obj, &k)?;
                stack.push(v);
            }
            Op::SetElemLocal(s) | Op::SetElemLocalDrop(s) => {
                let keep = matches!(op, Op::SetElemLocal(_));
                let v = pop!();
                let key = pop!();
                if keep {
                    stack.push(v.clone());
                }
                if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                    match i.fast_set_elem(o, *n, v) {
                        Ok(()) => continue,
                        Err(back) => {
                            let obj = slots[s as usize].clone();
                            let k = i.to_property_key(&key)?;
                            i.set_member(&obj, &k, back)?;
                            continue;
                        }
                    }
                }
                let obj = slots[s as usize].clone();
                let k = i.to_property_key(&key)?;
                i.set_member(&obj, &k, v)?;
            }
            Op::UpdateProp(n, c, kind) => {
                let obj = pop!();
                let name = &chunk.names[n as usize];
                let cache = &chunk.caches[c as usize];
                let old = i.get_prop_ic(&obj, name, cache)?;
                step_and_store(i, &mut stack, kind, old, |i, v| {
                    i.set_prop_ic(&obj, name, v, cache)
                })?;
            }
            Op::UpdateElem(kind) => {
                let key = pop!();
                let obj = pop!();
                // Dense-element fast path: numeric key on a plain array/object.
                if let (Value::Obj(o), Value::Num(nk)) = (&obj, &key) {
                    if let Some(Value::Num(old)) = i.fast_get_elem(o, *nk) {
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => old + 1.0,
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => old - 1.0,
                        };
                        if i.fast_set_elem(o, *nk, Value::Num(new)).is_ok() {
                            match kind {
                                UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::Num(new)),
                                UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::Num(old)),
                                UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                            }
                            continue;
                        }
                    }
                }
                // General path: nullish check, one ToPropertyKey, [[Get]], ToNumeric, [[Set]] —
                // the oracle's Reference order exactly.
                if matches!(obj, Value::Undefined | Value::Null) {
                    return Err(i.throw("TypeError", i.null_read_message(&obj, Some(&key))));
                }
                let k = i.to_property_key(&key)?;
                let old = i.get_member(&obj, &k)?;
                step_and_store(i, &mut stack, kind, old, |i, v| i.set_member(&obj, &k, v))?;
            }
            Op::ToPropKeyLocal(s) => match stack.last().expect("vm stack underflow") {
                Value::Num(_) | Value::Str(_) => {}
                _ => {
                    let key = pop!();
                    if matches!(slots[s as usize], Value::Undefined | Value::Null) {
                        return Err(
                            i.throw("TypeError", "cannot access property of null or undefined")
                        );
                    }
                    let k = i.to_property_key(&key)?;
                    stack.push(Value::str(k));
                }
            },
            Op::ToPropKey => {
                match stack.last().expect("vm stack underflow") {
                    // Side-effect-free and deterministic to coerce later; numbers stay numeric
                    // so GetElem/SetElem keep their dense fast path.
                    Value::Num(_) | Value::Str(_) => {}
                    _ => {
                        let key = pop!();
                        if matches!(
                            stack.last().expect("vm stack underflow"),
                            Value::Undefined | Value::Null
                        ) {
                            return Err(i.throw(
                                "TypeError",
                                "cannot access property of null or undefined",
                            ));
                        }
                        let k = i.to_property_key(&key)?;
                        stack.push(Value::str(k));
                    }
                }
            }
            Op::Dup2 => {
                let len = stack.len();
                let a = stack[len - 2].clone();
                let b = stack[len - 1].clone();
                stack.push(a);
                stack.push(b);
            }
            Op::GetMethod(n, c) => {
                let obj = pop!();
                let m = i.get_prop_ic(&obj, &chunk.names[n as usize], &chunk.caches[c as usize])?;
                stack.push(obj);
                stack.push(m);
            }
            Op::GetMethodElem => {
                let key = pop!();
                let obj = pop!();
                let m = if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    match i.fast_get_elem(o, *n) {
                        Some(v) => v,
                        None => {
                            let k = i.to_property_key(&key)?;
                            i.get_member(&obj, &k)?
                        }
                    }
                } else {
                    if matches!(obj, Value::Undefined | Value::Null) {
                        return Err(i.throw("TypeError", i.null_read_message(&obj, Some(&key))));
                    }
                    let k = i.to_property_key(&key)?;
                    i.get_member(&obj, &k)?
                };
                stack.push(obj);
                stack.push(m);
            }
            Op::Add => bin_num(i, &mut stack, "+", |a, b| a + b)?,
            Op::Sub => bin_num(i, &mut stack, "-", |a, b| a - b)?,
            Op::Mul => bin_num(i, &mut stack, "*", |a, b| a * b)?,
            Op::Div => bin_num(i, &mut stack, "/", |a, b| a / b)?,
            Op::Mod => bin_num(i, &mut stack, "%", crate::eval::js_mod)?,
            Op::BitAnd => bin_i32(i, &mut stack, "&", |a, b| a & b)?,
            Op::BitOr => bin_i32(i, &mut stack, "|", |a, b| a | b)?,
            Op::BitXor => bin_i32(i, &mut stack, "^", |a, b| a ^ b)?,
            Op::Shl => bin_i32(i, &mut stack, "<<", |a, b| a.wrapping_shl(b as u32 & 31))?,
            Op::Shr => bin_i32(i, &mut stack, ">>", |a, b| a >> (b as u32 & 31))?,
            Op::UShr => {
                let b = pop!();
                let a = pop!();
                if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
                    let r = (crate::eval::to_int32(*x) as u32)
                        >> (crate::eval::to_int32(*y) as u32 & 31);
                    stack.push(Value::Num(r as f64));
                } else {
                    let v = i.binary(">>>", a, b)?;
                    stack.push(v);
                }
            }
            Op::Lt => bin_cmp(i, &mut stack, "<", |a, b| a < b)?,
            Op::Gt => bin_cmp(i, &mut stack, ">", |a, b| a > b)?,
            Op::Le => bin_cmp(i, &mut stack, "<=", |a, b| a <= b)?,
            Op::Ge => bin_cmp(i, &mut stack, ">=", |a, b| a >= b)?,
            Op::EqEq => bin_cmp(i, &mut stack, "==", |a, b| a == b)?,
            Op::NotEq => bin_cmp(i, &mut stack, "!=", |a, b| a != b)?,
            Op::StrictEq => strict_cmp(i, &mut stack, false),
            Op::StrictNotEq => strict_cmp(i, &mut stack, true),
            Op::InstanceOf(c) => {
                let b = pop!();
                let a = pop!();
                let v = i.instanceof_ic(&a, &b, &chunk.caches[c as usize])?;
                stack.push(v);
            }
            Op::GenBin(n) => {
                let b = pop!();
                let a = pop!();
                let v = i.binary(&chunk.names[n as usize], a, b)?;
                stack.push(v);
            }
            Op::Neg | Op::Plus | Op::Not | Op::BitNot | Op::Typeof => {
                let operator = match op {
                    Op::Neg => "-",
                    Op::Plus => "+",
                    Op::Not => "!",
                    Op::BitNot => "~",
                    Op::Typeof => "typeof",
                    _ => unreachable!(),
                };
                let value = pop!();
                stack.push(crate::native_ops::unary(i, operator, value)?);
            }
            Op::TypeofIs(kind, negated) => {
                let a = pop!();
                let b = (TypeofKind::of(i, &a) == kind) != negated;
                vm_regs::drop_fast(a);
                stack.push_bool(b);
            }
            Op::TypeofName(n) => {
                stack.push(i.typeof_name_vm(&chunk.names[n as usize], env)?);
            }
            Op::Void => {
                let value = pop!();
                stack.push(crate::native_ops::unary(i, "void", value)?);
            }
            Op::Jump(t) => {
                // A backward jump is a loop turn: the same safe point the tree-walker runs at every
                // loop head, amortized, so an allocation-free `while (true) {}` still reaches the
                // collector and the embedder's interrupt.
                if (t as usize) < *pc {
                    i.gc_check_amortized()?;
                    let backedge = *pc - 1;
                    *pc = t as usize;
                    if jit::backedge_due(i, chunk) {
                        // Through a temporary: taking `pc`'s address would pin it to memory.
                        let mut p = *pc;
                        let r = jit::on_backedge(
                            i,
                            chunk,
                            env,
                            slots,
                            stack.vec(),
                            &mut p,
                            this_val,
                            backedge,
                        );
                        unsafe { stack.reload() };
                        *pc = p;
                        if let Some(step) = r? {
                            return Ok(step);
                        }
                    }
                    continue;
                }
                *pc = t as usize
            }
            Op::JumpIfFalse(t) => {
                let yes = match stack.last() {
                    Some(Value::Bool(b)) => {
                        let b = *b;
                        unsafe { stack.forget_top(1) };
                        b
                    }
                    _ => {
                        let a = pop!();
                        let b = i.to_boolean(&a);
                        vm_regs::drop_fast(a);
                        b
                    }
                };
                if !yes {
                    *pc = t as usize;
                }
            }
            Op::JumpIfNotCmp(kind, t) => {
                let yes = match (stack.peek(1), stack.peek(0)) {
                    (Value::Num(x), Value::Num(y)) => {
                        let r = kind.num(*x, *y);
                        unsafe { stack.forget_top(2) };
                        r
                    }
                    (x, y) if kind.is_strict() => {
                        let r = i.strict_equals(x, y) == matches!(kind, CmpKind::StrictEq);
                        let b = pop!();
                        vm_regs::drop_fast(b);
                        let a = pop!();
                        vm_regs::drop_fast(a);
                        r
                    }
                    _ => {
                        let b = pop!();
                        let a = pop!();
                        let v = i.binary(kind.name(), a, b)?;
                        i.to_boolean(&v)
                    }
                };
                if !yes {
                    *pc = t as usize;
                }
            }
            Op::ArithLL(kind, d, a, b) => {
                if kind == ArithKind::Add
                    && d == a
                    && d != b
                    && matches!(slots[a as usize], Value::Str(_))
                {
                    let y = slots[b as usize].clone();
                    if append_to_slot(slots, d as usize, &y) {
                        continue;
                    }
                }
                match (&slots[a as usize], &slots[b as usize]) {
                    (Value::Num(x), Value::Num(y)) => {
                        let r = kind.num(*x, *y);
                        vm_regs::set_num(&mut slots[d as usize], r);
                    }
                    (x, y) => {
                        let (x, y) = (x.clone(), y.clone());
                        slots[d as usize] = arith_slow(i, chunk, kind, x, y, a, Some(b))?;
                    }
                }
            }
            Op::ArithLK(kind, d, a, k) => {
                if kind == ArithKind::Add
                    && d == a
                    && matches!(slots[a as usize], Value::Str(_))
                    && append_to_slot(slots, d as usize, &chunk.consts[k as usize])
                {
                    continue;
                }
                match (&slots[a as usize], &chunk.consts[k as usize]) {
                    (Value::Num(x), Value::Num(y)) => {
                        let r = kind.num(*x, *y);
                        vm_regs::set_num(&mut slots[d as usize], r);
                    }
                    (x, y) => {
                        let (x, y) = (x.clone(), y.clone());
                        slots[d as usize] = arith_slow(i, chunk, kind, x, y, a, None)?;
                    }
                }
            }
            Op::JumpIfNotCmpLL(kind, a, b, t) => {
                let yes = match (&slots[a as usize], &slots[b as usize]) {
                    (Value::Num(x), Value::Num(y)) => kind.num(*x, *y),
                    (x, y)
                        if kind.is_strict()
                            && !matches!(x, Value::Empty)
                            && !matches!(y, Value::Empty) =>
                    {
                        i.strict_equals(x, y) == matches!(kind, CmpKind::StrictEq)
                    }
                    (x, y) => {
                        let (x, y) = (x.clone(), y.clone());
                        cmp_slow(i, chunk, kind, x, y, a, Some(b))?
                    }
                };
                if !yes {
                    *pc = t as usize;
                }
            }
            Op::JumpIfNotCmpLK(kind, a, k, t) => {
                let yes = match (&slots[a as usize], &chunk.consts[k as usize]) {
                    (Value::Num(x), Value::Num(y)) => kind.num(*x, *y),
                    (x, y) if kind.is_strict() && !matches!(x, Value::Empty) => {
                        i.strict_equals(x, y) == matches!(kind, CmpKind::StrictEq)
                    }
                    (x, y) => {
                        let (x, y) = (x.clone(), y.clone());
                        cmp_slow(i, chunk, kind, x, y, a, None)?
                    }
                };
                if !yes {
                    *pc = t as usize;
                }
            }
            Op::JumpIfFalsePeek(t) => {
                if !i.to_boolean(stack.last().expect("vm stack underflow")) {
                    *pc = t as usize;
                }
            }
            Op::JumpIfTruePeek(t) => {
                if i.to_boolean(stack.last().expect("vm stack underflow")) {
                    *pc = t as usize;
                }
            }
            Op::JumpIfNotNullishPeek(t) => {
                if !matches!(
                    stack.last().expect("vm stack underflow"),
                    Value::Undefined | Value::Null
                ) {
                    *pc = t as usize;
                }
            }
            // Calls pass the argument window as a slice of the operand stack — no per-call `Vec`.
            // The callee/receiver slots below the window are cloned out first, then the whole
            // region is truncated away after the call. On a throw the stack is left long, which is
            // fine: the handler unwind (or function exit) truncates it.
            Op::Call(argc) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                let at = stack.len() - argc as usize;
                try_inline_call!(at, false);
                let callee = stack[at - 1].clone();
                // `await f()`: an async callee may skip its promise (see `note_await_call`).
                let fused = matches!(ops.get(*pc), Some(Op::Await));
                if fused {
                    i.note_await_call(&callee);
                }
                let v = i.call(callee, Value::Undefined, &stack[at..]);
                if fused {
                    i.clear_await_call();
                }
                let v = v.map_err(|e| call_error(i, chunk, *pc - 1, &stack[at - 1], e))?;
                stack.truncate(at - 1);
                stack.push(v);
            }
            Op::LoadNameForCall(n, c) => {
                // A depth-0 cache hit/fill can't have come through a `with` object: `this` is
                // undefined. Only the full walk can produce a with-object receiver.
                if let Some(v) = chunk
                    .name_ic_hit(i, env, c)
                    .or_else(|| chunk.name_ic_fill(i, env, n, c))
                {
                    stack.push(Value::Undefined);
                    stack.push(v);
                } else {
                    let (callee, with_this) = i.get_var_with(&chunk.names[n as usize], env)?;
                    stack.push(with_this.unwrap_or(Value::Undefined));
                    stack.push(callee);
                }
            }
            Op::CallWithThis(argc) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                let at = stack.len() - argc as usize;
                try_inline_call!(at, true);
                let m = stack[at - 1].clone();
                let this = stack[at - 2].clone();
                let fused = matches!(ops.get(*pc), Some(Op::Await));
                if fused {
                    i.note_await_call(&m);
                }
                let v = i.call(m, this, &stack[at..]);
                if fused {
                    i.clear_await_call();
                }
                let v = v.map_err(|e| call_error(i, chunk, *pc - 1, &stack[at - 1], e))?;
                stack.truncate(at - 2);
                stack.push(v);
            }
            Op::New(argc) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                let at = stack.len() - argc as usize;
                // A constructor template: the instance without running the body.
                if let Some(r) =
                    ctor_plan::construct_with_plan(i, &stack[at - 1], &stack[at - 1], &stack[at..])
                {
                    let v = r?;
                    stack.truncate(at - 1);
                    stack.push(v);
                    continue;
                }
                if !ONE && !frames.is_null() {
                    if let Some(c) = inline_ctor(i, &stack[at - 1])
                        .or_else(|| class_fields::inline_class_ctor(i, &stack[at - 1]))
                    {
                        stack.sync();
                        unsafe { (*frames).reserve_one(i) };
                        match unsafe { (*frames).top() } {
                            Some(f) => {
                                stack.repoint(&mut f.stack);
                                pc.dst = &mut f.pc;
                            }
                            None => {
                                stack.repoint(root_stack);
                                pc.dst = root_pc;
                            }
                        }
                        unsafe { enter_inline(i, frames, stack.vec(), at, false, true, c)? };
                        load_frame!();
                        jit_entry!();
                        continue;
                    }
                }
                let callee = stack[at - 1].clone();
                let v = i.construct(callee, &stack[at..])?;
                stack.truncate(at - 1);
                stack.push(v);
            }
            Op::MakeRegExp(body, flags) => {
                stack.push(i.make_regexp_literal(
                    &chunk.names[body as usize],
                    &chunk.names[flags as usize],
                )?);
            }
            Op::MakeArray(n) => {
                let at = stack.len() - n as usize;
                // Elements move straight from the stack into the new array (no temporary Vec).
                let v = stack.with_vec(|s| i.make_array_iter(s.drain(at..)));
                stack.push(v);
            }
            Op::MakeObject(start, count, tidx) => {
                let at = stack.len() - count as usize;
                let keys = &chunk.names[start as usize..start as usize + count as usize];
                let v = if tidx != u32::MAX {
                    // Values move straight from the stack into the new object (no temporary Vec).
                    let tmpl = &chunk.obj_maps[tidx as usize];
                    stack.with_vec(|vals| {
                        i.make_plain_object_templated(tmpl, keys, vals.drain(at..))
                    })
                } else {
                    let values: Vec<Value> = stack.split_off(at);
                    i.make_plain_object_vm(keys, values)
                };
                stack.push(v);
            }
            Op::ToStr => {
                let v = pop!();
                let s = i.to_string(&v)?;
                stack.push(Value::Str(s));
            }
            Op::ForInKeys => {
                let base = pop!();
                let keys = i.for_in_keys(&base)?;
                stack.push(i.make_array(keys));
            }
            Op::ForInStepL(base, keys, cursor) => {
                let value = for_in::step(i, slots, base, keys, cursor)?;
                let more = value.is_some();
                stack.push(value.unwrap_or(Value::Undefined));
                stack.push(Value::Bool(more));
            }
            Op::GetIter => {
                let v = pop!();
                // A pristine Array/Map/Set: no iterator object (see `iter_fast`).
                if let Some(state) = iter_fast::open(i, &v) {
                    stack.push(Value::Num(state));
                    stack.push(v);
                } else if let Some((state, coll)) = iter_fast::open_coll_iter(i, &v) {
                    // A fresh `m.keys()`-style iterator only this operand holds.
                    drop(v);
                    stack.push(Value::Num(state));
                    stack.push(coll);
                } else {
                    let (it, nx) = i.get_iterator(&v)?;
                    stack.push(it);
                    stack.push(nx);
                }
            }
            Op::IterStepL(is, ns) => {
                // An encoded protocol-free state (see `iter_fast`); `None` falls through to the
                // protocol on the (possibly materialized) slots.
                if let Some(stepped) = iter_fast::step_slots(i, slots, is, ns) {
                    match stepped {
                        Some(v) => {
                            stack.push(v);
                            stack.push(Value::Bool(true));
                        }
                        None => {
                            stack.push(Value::Undefined);
                            stack.push(Value::Bool(false));
                        }
                    }
                    continue;
                }
                // The pure fast path borrows rooted slots; only fallback crosses a callback.
                let stepped = match array_iterator_step::try_yield(
                    i,
                    &slots[is as usize],
                    &slots[ns as usize],
                ) {
                    Some(value) => Some(value),
                    None => {
                        let it = slots[is as usize].clone();
                        let nx = slots[ns as usize].clone();
                        // Owned clones stay alive across reentrant next/done/value calls.
                        i.iterator_step(&it, &nx)?
                    }
                };
                match stepped {
                    Some(v) => {
                        stack.push(v);
                        stack.push(Value::Bool(true));
                    }
                    None => {
                        stack.push(Value::Undefined);
                        stack.push(Value::Bool(false));
                    }
                }
            }
            Op::IterCloseL(s) => {
                if !iter_fast::close_is_noop(i, slots, s) {
                    let it = slots[s as usize].clone();
                    i.iterator_close_normal(&it)?;
                }
            }
            Op::IterAbortL(s) => {
                let exc = pop!();
                if !iter_fast::close_is_noop(i, slots, s) {
                    let it = slots[s as usize].clone();
                    i.iterator_close(&it);
                }
                return Err(Abrupt::Throw(exc));
            }
            Op::Throw => {
                let v = pop!();
                return Err(Abrupt::Throw(v));
            }
            Op::Return => {
                let v = pop!();
                return_inline!(v);
                return Ok(VmStep::Done(v));
            }
            Op::ReturnUndef => {
                return_inline!(Value::Undefined);
                return Ok(VmStep::Done(Value::Undefined));
            }
            Op::Await => return Ok(VmStep::Await(pop!())),
            Op::PushHandler(catch_pc) => handlers.push(Handler {
                catch_pc: catch_pc as usize,
                stack_depth: stack.len(),
            }),
            Op::PopHandler => {
                handlers.pop();
            }
            Op::GetPrivate(n) => {
                stack.with_vec(|s| ext_ops::get_private(i, env, &chunk.names[n as usize], s))?
            }
            Op::SetPrivate(n) => {
                stack.with_vec(|s| ext_ops::set_private(i, env, &chunk.names[n as usize], s))?
            }
            Op::GetPrivateMethod(n) => stack
                .with_vec(|s| ext_ops::get_private_method(i, env, &chunk.names[n as usize], s))?,
            Op::PrivateIn(n) => {
                stack.with_vec(|s| ext_ops::private_in(i, env, &chunk.names[n as usize], s))?
            }
            Op::UpdatePrivate(n, kind) => stack
                .with_vec(|s| ext_ops::update_private(i, env, &chunk.names[n as usize], kind, s))?,
            Op::NewObject => stack.push(Value::Obj(i.new_object())),
            Op::InitProp(n, named) => {
                stack.with_vec(|s| ext_ops::init_prop(i, &chunk.names[n as usize], named, s))?
            }
            Op::InitPropComputed(named) => {
                stack.with_vec(|s| ext_ops::init_prop_computed(i, named, s))?
            }
            Op::DefineField(n, named) => {
                let v = pop!();
                let this = pop!();
                class_fields::define_field(i, this, &chunk.names[n as usize], named, v)?
            }
            Op::InitMethod(fidx, n, kind) => {
                let key = (n != u32::MAX).then(|| &*chunk.names[n as usize]);
                stack.with_vec(|s| {
                    ext_ops::init_method(i, env, &chunk.funcs[fidx as usize], key, kind, s)
                })?
            }
            Op::CopyDataProps => stack.with_vec(|s| ext_ops::copy_data_props(i, s))?,
            Op::SetProtoLit => stack.with_vec(|s| ext_ops::set_proto_lit(s)),
            Op::MakeClass(cidx, n) => {
                let name = (n != u32::MAX).then(|| &*chunk.names[n as usize]);
                stack.with_vec(|s| {
                    ext_ops::make_class(i, env, &chunk.classes[cidx as usize], name, s)
                })?
            }
            Op::Nip => {
                let top = pop!();
                pop!();
                stack.push(top);
            }
            Op::SuperGet(n) => {
                stack.with_vec(|s| ext_ops::super_get(i, env, Some(&chunk.names[n as usize]), s))?
            }
            Op::SuperGetElem => stack.with_vec(|s| ext_ops::super_get(i, env, None, s))?,
            Op::SuperBase => stack.with_vec(|s| ext_ops::super_base(i, env, s))?,
            Op::SuperMethod(n, lexical) => stack.with_vec(|s| {
                ext_ops::super_method(i, env, this_val, Some(&chunk.names[n as usize]), lexical, s)
            })?,
            Op::SuperMethodElem(lexical) => {
                stack.with_vec(|s| ext_ops::super_method(i, env, this_val, None, lexical, s))?
            }
            Op::ObjRest(start, count) => {
                let keys = &chunk.names[start as usize..start as usize + count as usize];
                stack.with_vec(|s| ext_ops::obj_rest(i, keys, s))?
            }
            Op::NewArrayLit => stack.push(i.make_array(Vec::new())),
            Op::ArrayAppend => {
                let v = pop!();
                stack.with_vec(|s| match s.last() {
                    Some(Value::Obj(ao)) => ext_ops::append_one(ao, v),
                    _ => unreachable!("array literal under construction"),
                });
            }
            Op::ArrayAppendSpread => {
                let v = pop!();
                let items = match iter_fast::values(i, &v) {
                    Some(items) => items,
                    None => i.iterate(&v)?,
                };
                // Still empty (`[...xs]`, or the first spread of a literal): no code has seen
                // the literal, so a fresh array built from the items in one piece replaces it.
                let empty = matches!(stack.last(), Some(Value::Obj(a)) if matches!(
                    a.borrow().props.length_property().map(|p| p.value()),
                    Some(Value::Num(n)) if n == 0.0
                ));
                if empty && !items.is_empty() {
                    let arr = i.make_array(items);
                    drop(pop!());
                    stack.push(arr);
                } else {
                    stack.with_vec(|s| ext_ops::array_append(s, items, false));
                }
            }
            Op::ArrayHole => stack.with_vec(|s| ext_ops::array_append(s, std::iter::empty(), true)),
            Op::ArrayCbGuard(k) => stack.with_vec(|s| inline_callback::guard(i, k, *pc - 1, s)),
            Op::ArrayCbHas => {
                let k = pop!();
                let a = pop!();
                let present = inline_callback::has(i, &a, &k)?;
                stack.push_bool(present);
            }
            Op::ArrayCbDone(pos) => i.cur_site = pos,
            Op::SwitchLK(s, t) => {
                let table = chunk.switch_tables[t as usize].get_or_init(|| {
                    switch_table::SwitchTable::build(&chunk.ops, *pc, s, &chunk.consts)
                });
                if let Some(to) = table.target(&slots[s as usize]) {
                    *pc = to as usize;
                }
            }
            Op::SuperCtor => stack.with_vec(|s| derived::super_ctor(i, env, s))?,
            Op::SuperCall(n) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                stack.with_vec(|s| derived::super_call(i, env, s, n as usize, false))?
            }
            Op::SuperCallSpread(n) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                stack.with_vec(|s| derived::super_call(i, env, s, n as usize, true))?
            }
            Op::DerivedReturn => {
                let v = pop!();
                match i.derived_construct_result(env, v) {
                    Ok(v) => {
                        return_inline!(v);
                        return Ok(VmStep::Done(v));
                    }
                    Err(e) => {
                        // [[Construct]]'s own error: no handler of the body may catch it.
                        handlers.clear();
                        return Err(e);
                    }
                }
            }
            // Generator suspensions travel as `Await` steps; `VmCoro` tells them apart by the op
            // it parked at.
            Op::InitialYield => return Ok(VmStep::Await(Value::Undefined)),
            Op::Yield => return Ok(VmStep::Await(pop!())),
            Op::YieldDelegate(it) => {
                iter_fast::materialize_slots(i, slots, it);
                if let Some(result) =
                    stack.with_vec(|s| generator::delegate_step(i, slots, it, s))?
                {
                    *pc -= 1; // park on the op itself: the resume re-runs it
                    return Ok(VmStep::Await(result));
                }
            }
            Op::LoadNewTarget => stack.push(i.new_target.clone()),
            Op::Concat(n) => {
                let at = stack.len() - n as usize;
                let v = stack.with_vec(|s| {
                    let r = concat_strs(i, &s[at..]);
                    s.truncate(at);
                    r
                })?;
                stack.push(v);
            }
            Op::LoadCallee => {
                jit::sync_frames(i);
                let f = i
                    .fn_frames
                    .last()
                    .expect("a function body runs inside its FnFrame")
                    .callee();
                stack.push(Value::Obj(f));
            }
            Op::BlkNew(dst, parent) => block_env::new_env(env, slots, dst, parent),
            Op::BlkDecl(s, n, k) => block_env::declare(slots, s, &chunk.names[n as usize], k),
            Op::BlkCopy(s) => block_env::copy(slots, s),
            Op::BlkLoad(s, n) => {
                let v = block_env::load(i, slots, s, &chunk.names[n as usize])?;
                stack.push(v);
            }
            Op::BlkStore(s, n) => {
                let v = pop!();
                block_env::store(i, slots, s, &chunk.names[n as usize], v, false)?;
            }
            Op::BlkInit(s, n) => {
                let v = pop!();
                block_env::store(i, slots, s, &chunk.names[n as usize], v, true)?;
            }
            Op::BlkUpdate(s, n, kind) => {
                let name = &chunk.names[n as usize];
                match block_env::update_num(slots, s, name, kind) {
                    Some(Some(v)) => stack.push(Value::Num(v)),
                    Some(None) => {}
                    None => block_env::update(i, &mut stack, slots, s, name, kind)?,
                }
            }
            Op::InEnv(s) => {
                let benv = block_env::env_of(&slots[s as usize]);
                let next = ops[*pc];
                *pc += 1;
                match next {
                    Op::MakeClosure(fidx, name_n) => {
                        let v = i.make_function(chunk.funcs[fidx as usize].clone(), benv);
                        if name_n != u32::MAX {
                            i.set_fn_name(&v, &chunk.names[name_n as usize]);
                        }
                        stack.push(v);
                    }
                    Op::InitMethod(fidx, n, kind) => {
                        let key = (n != u32::MAX).then(|| &*chunk.names[n as usize]);
                        stack.with_vec(|s| {
                            ext_ops::init_method(
                                i,
                                &benv,
                                &chunk.funcs[fidx as usize],
                                key,
                                kind,
                                s,
                            )
                        })?
                    }
                    Op::MakeClass(cidx, n) => {
                        let name = (n != u32::MAX).then(|| &*chunk.names[n as usize]);
                        stack.with_vec(|s| {
                            ext_ops::make_class(i, &benv, &chunk.classes[cidx as usize], name, s)
                        })?
                    }
                    _ => unreachable!("InEnv prefixes a closure-creating op"),
                }
            }
            Op::TailCall(argc, has_this) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                tail_call!(argc as usize, has_this);
            }
            Op::TailCallSpread(argc, has_this) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                // Expand the spread onto the stack: the window becomes an ordinary one.
                let spread = pop!();
                let mut n = argc as usize - 1;
                if !iter_fast::array_elems(i, &spread, |x| {
                    stack.push(x);
                    n += 1;
                }) {
                    let (it, nx) = i.get_iterator(&spread)?;
                    while let Some(x) = i.iterator_step(&it, &nx)? {
                        stack.push(x);
                        n += 1;
                    }
                }
                tail_call!(n, has_this);
            }
            Op::ImportCall(phase, has_opts) => {
                let opts = if has_opts { Some(pop!()) } else { None };
                let spec = pop!();
                let phase = match phase {
                    1 => crate::ast::ImportPhase::Source,
                    2 => crate::ast::ImportPhase::Defer,
                    _ => crate::ast::ImportPhase::Evaluation,
                };
                let v = i.import_call_finish(spec, opts, phase, env)?;
                stack.push(v);
            }
            Op::GetAsyncIter => {
                let rhs = pop!();
                for_await::get_async_iter(i, &mut stack, rhs)?;
            }
            Op::AsyncIterNext(it, nx, st) => {
                let v = for_await::next(i, slots, it, nx, st)?;
                stack.push(v);
            }
            Op::AsyncIterResult(st) => {
                let v = pop!();
                for_await::result(i, &mut stack, slots, st, v)?;
            }
            Op::AsyncCloseCall(it, st) => for_await::close_call(i, &mut stack, slots, it, st)?,
            Op::AsyncCloseCheck(st) => {
                let v = pop!();
                for_await::close_check(i, slots, st, v)?;
            }
            Op::AsyncDelegateInit => {
                let v = pop!();
                generator::async_init(i, &mut stack, v)?;
            }
            Op::AsyncDelegateCall(it, md, rt) => {
                let v = pop!();
                generator::async_call(i, &mut stack, slots, it, md, rt, v)?;
            }
            Op::AsyncDelegateResult(fs, dn) => {
                let v = pop!();
                generator::async_result(i, &mut stack, slots, fs, dn, v)?;
            }
            Op::AsyncDelegateCloseReject(it, dn) => {
                let exc = pop!();
                return Err(generator::async_close_reject(i, slots, it, dn, exc));
            }
            Op::AsyncDelegateSpecial(md, err) => {
                let e = if err { Some(pop!()) } else { None };
                generator::async_special(i, slots, md, e)?;
            }
            Op::IterRestL(it, nx) => {
                let v = destructure_seq::rest(i, slots, it, nx)?;
                stack.push(v);
            }
            Op::NewSpread(argc) => {
                i.cur_site = crate::interpreter::frames::SITE_PC | (*pc - 1) as u32;
                let spread = pop!();
                let at = stack.len() - (argc as usize - 1);
                let mut args: Vec<Value> = stack.split_off(at);
                if !iter_fast::array_elems(i, &spread, |x| args.push(x)) {
                    let (it, nx) = i.get_iterator(&spread)?;
                    while let Some(x) = i.iterator_step(&it, &nx)? {
                        args.push(x);
                    }
                }
                let callee = pop!();
                let v = i.construct(callee, &args)?;
                stack.push(v);
            }
        }
    }
}

/// An async function body — or a sync generator body (see [`generator`]) — running on the
/// bytecode VM, suspendable at each `await` / `yield` without an OS thread. It presents the same
/// `resume(&mut Interp, Resume) -> Suspend` shape as the thread-backed coroutine, so the promise
/// driver (`Interp::drive_async`) and the generator driver treat both uniformly — the only cost
/// per suspension is now a couple of `Vec` swaps instead of a thread handoff.
pub struct VmCoro {
    chunk: Rc<Chunk>,
    env: Env,
    this_val: Value,
    slots: Vec<Value>,
    stack: Vec<Value>,
    pc: u32,
    /// The `try` handler stack, saved across suspensions so a rejected `await` inside a `try` still
    /// lands in its `catch`. Boxed: most suspensions sit outside any `try`.
    handlers: Option<Box<Vec<Handler>>>,
    pub done: bool,
    pub started: bool,
    /// A generator body's strictness, installed on the interpreter for each resume (a resume
    /// runs from `next()` in whatever context called it). `None` for async bodies.
    strict: Option<bool>,
    /// The stack-trace frame each resume runs under (see `Coroutine::set_frame`).
    pub(crate) frame: crate::interpreter::stack_trace::ResumeFrame,
}

/// Where a suspended [`VmCoro`] is parked (from the op at / before its pc).
#[derive(PartialEq)]
enum Parked {
    /// Before the first statement of a generator (or an async body not yet started).
    Start,
    /// At an `await`: the resume value is the settled result.
    Await,
    /// At a `yield`: the resume pushes `[received, returning]`.
    Yield,
    /// At a `yield*` step: the resume pushes `[received, mode]` and re-runs it.
    Delegate,
}

impl VmCoro {
    /// Build an async coroutine for `chunk` with params seeded from `args`, parked before its first
    /// step (run on the first `resume`).
    pub fn new(
        i: &mut Interp,
        chunk: Rc<Chunk>,
        env: Env,
        this_val: Value,
        args: &[Value],
    ) -> VmCoro {
        let env = chunk.make_run_env(i, &env, &this_val, args);
        // Buffers come from the VM pool (see [`run`]) and go back when the body finishes.
        let (mut slots, stack) = i.vm_pool.pop().unwrap_or_default();
        // A suspended body keeps its slots: size them exactly rather than to a pooled frame's.
        if slots.capacity() > chunk.n_slots + 4 {
            slots = Vec::new();
        }
        slots.reserve_exact(chunk.n_slots);
        let seed = chunk.n_params.min(args.len());
        slots.extend_from_slice(&args[..seed]);
        slots.resize(chunk.n_slots, Value::Undefined);
        chunk.seed_rest(i, &mut slots, args);
        for &s in &chunk.var_force_resets {
            slots[s as usize] = Value::Undefined;
        }
        VmCoro {
            chunk,
            env,
            this_val,
            slots,
            stack,
            pc: 0,
            handlers: None,
            done: false,
            started: false,
            strict: None,
            frame: Default::default(),
        }
    }

    /// Start a sync generator call: seed the frame and run its prologue (parameter
    /// initialization, declaration instantiation) up to `Op::InitialYield`, synchronously — a
    /// throw there propagates from the call. The coroutine is then suspendedStart. Must run
    /// inside the generator function's own call (its `FnFrame` supplies `arguments`' callee).
    pub(crate) fn new_generator(
        i: &mut Interp,
        chunk: Rc<Chunk>,
        env: Env,
        this_val: Value,
        args: &[Value],
        strict: bool,
    ) -> Result<VmCoro, Abrupt> {
        let mut coro = VmCoro::new(i, chunk, env, this_val, args);
        coro.strict = Some(strict);
        if let (Some(s), None) = (coro.chunk.arguments_slot, coro.chunk.virt_base) {
            coro.slots[s as usize] = Value::Obj(i.make_compiled_arguments_object(args, &coro.env));
        }
        let saved = (i.strict, i.tco_ok);
        i.strict = strict;
        i.tco_ok = false;
        let mut pc = coro.pc as usize;
        let mut handlers = Vec::new();
        let r = drive_vm(
            i,
            &coro.chunk,
            &coro.env,
            &mut coro.slots,
            &mut coro.stack,
            &mut pc,
            &coro.this_val,
            &mut handlers,
            None,
        );
        coro.pc = pc as u32;
        coro.save_handlers(handlers);
        (i.strict, i.tco_ok) = saved;
        match r? {
            VmStep::Await(_) => {
                debug_assert!(coro.parked() == Parked::Start);
                Ok(coro)
            }
            VmStep::Done(_) => unreachable!("a generator prologue ends at InitialYield"),
        }
    }

    /// [`VmCoro::parked`] at a suspension, first giving back an empty operand stack's buffer:
    /// a suspended body rarely holds operands, and thousands of pending ones would otherwise
    /// each keep a grown buffer.
    fn save_handlers(&mut self, handlers: Vec<Handler>) {
        if !handlers.is_empty() {
            self.handlers = Some(Box::new(handlers));
        }
    }

    fn parked_shrunk(&mut self) -> Parked {
        if self.stack.is_empty() && self.stack.capacity() != 0 {
            self.stack = Vec::new();
        }
        self.parked()
    }

    fn parked(&self) -> Parked {
        let ops = &self.chunk.ops;
        let pc = self.pc as usize;
        if matches!(ops.get(pc), Some(Op::YieldDelegate(_))) {
            return Parked::Delegate;
        }
        match pc.checked_sub(1).map(|k| &ops[k]) {
            Some(Op::Yield) => Parked::Yield,
            Some(Op::InitialYield) | None => Parked::Start,
            _ => Parked::Await,
        }
    }

    /// Drive one step: run to the next `await` (`Suspend::Await`) / `yield` (`Suspend::Yield`), to
    /// completion (`Done`), or to an uncaught throw (`Throw`). A `Resume::Throw` (rejected await,
    /// `gen.throw(e)`) is injected at the suspension point so an enclosing `try`/`catch` in the
    /// body can catch it; a `Resume::Return` at a yield completes like `return v` from there
    /// (running `finally` blocks); only uncaught completions finish the body.
    pub fn resume(
        &mut self,
        i: &mut Interp,
        signal: crate::coroutine::Resume,
    ) -> crate::coroutine::Suspend {
        use crate::coroutine::{Resume, Suspend};
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        let parked = self.parked();
        if self.stack.capacity() == 0 {
            self.stack.reserve_exact(16);
        }
        let pending_throw = match signal {
            Resume::Next(v) => {
                match parked {
                    Parked::Start if !self.started => {}
                    Parked::Yield => {
                        self.stack.push(v);
                        self.stack.push(Value::Bool(false));
                    }
                    Parked::Delegate => {
                        self.stack.push(v);
                        self.stack.push(Value::Num(0.0));
                    }
                    // The settled value of the await we parked at.
                    _ => self.stack.push(v),
                }
                None
            }
            Resume::Throw(e) if self.started && parked == Parked::Delegate => {
                self.stack.push(e);
                self.stack.push(Value::Num(1.0));
                None
            }
            // A rejected await / `gen.throw(e)`: re-enter the VM throwing `e` at the suspension.
            Resume::Throw(e) if self.started => Some(e),
            Resume::Throw(e) => {
                self.done = true;
                self.release_to(i);
                return Suspend::Throw(e);
            }
            Resume::Return(v) if self.started && parked == Parked::Yield => {
                self.stack.push(v);
                self.stack.push(Value::Bool(true));
                None
            }
            Resume::Return(v) if self.started && parked == Parked::Delegate => {
                self.stack.push(v);
                self.stack.push(Value::Num(2.0));
                None
            }
            Resume::Return(v) => {
                self.done = true;
                self.release_to(i);
                return Suspend::Done(v);
            }
        };
        self.started = true;
        let saved = (i.strict, i.tco_ok);
        if let Some(strict) = self.strict {
            i.strict = strict;
            i.tco_ok = false;
        }
        let mut pc = self.pc as usize;
        let mut handlers = self.handlers.take().map_or_else(Vec::new, |h| *h);
        let r = drive_vm(
            i,
            &self.chunk,
            &self.env,
            &mut self.slots,
            &mut self.stack,
            &mut pc,
            &self.this_val,
            &mut handlers,
            pending_throw,
        );
        self.pc = pc as u32;
        self.save_handlers(handlers);
        if self.strict.is_some() {
            (i.strict, i.tco_ok) = saved;
        }
        match r {
            Ok(VmStep::Await(a)) => match self.parked_shrunk() {
                Parked::Yield | Parked::Delegate => Suspend::Yield(a),
                _ => Suspend::Await(a),
            },
            Ok(VmStep::Done(v)) => {
                self.done = true;
                self.release_to(i);
                Suspend::Done(v)
            }
            Err(Abrupt::Throw(e)) => {
                self.done = true;
                self.release_to(i);
                Suspend::Throw(e)
            }
            // Return/Break/Continue can't escape a function body; treat defensively as completion.
            Err(_) => {
                self.done = true;
                self.release_to(i);
                Suspend::Done(Value::Undefined)
            }
        }
    }

    /// A finished body keeps nothing alive (a done generator object may live on indefinitely).
    /// Its slot and stack buffers return to the VM pool.
    fn release_to(&mut self, i: &mut Interp) {
        let mut slots = std::mem::take(&mut self.slots);
        let mut stack = std::mem::take(&mut self.stack);
        slots.clear();
        stack.clear();
        if slots.capacity() != 0 && i.vm_pool.len() < 64 {
            i.vm_pool.push((slots, stack));
        }
        self.handlers = None;
        self.this_val = Value::Undefined;
    }
}

/// Shared `++`/`--` tail for property/element updates: ToNumeric the old value, write old±1 back
/// through `set`, and return the value to leave on the stack — old / new / nothing per `kind`.
/// Post variants yield the *coerced* old value, matching the oracle's `eval_update`; a BigInt
/// stays a BigInt.
fn step_value(
    i: &mut Interp,
    kind: UpdKind,
    old: Value,
    set: impl FnOnce(&mut Interp, Value) -> Result<(), Abrupt>,
) -> Result<Option<Value>, Abrupt> {
    let inc = matches!(
        kind,
        UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard
    );
    Ok(match old {
        Value::BigInt(n) => {
            let new = match n.checked_step(inc) {
                Ok(v) => v,
                Err(e) => return Err(i.throw("RangeError", e.message())),
            };
            set(i, Value::BigInt(new.clone()))?;
            match kind {
                UpdKind::PreInc | UpdKind::PreDec => Some(Value::BigInt(new)),
                UpdKind::PostInc | UpdKind::PostDec => Some(Value::BigInt(n)),
                UpdKind::IncDiscard | UpdKind::DecDiscard => None,
            }
        }
        other => {
            let oldn = match other {
                Value::Num(n) => n,
                other => i.to_number(&other)?,
            };
            let new = if inc { oldn + 1.0 } else { oldn - 1.0 };
            set(i, Value::Num(new))?;
            match kind {
                UpdKind::PreInc | UpdKind::PreDec => Some(Value::Num(new)),
                UpdKind::PostInc | UpdKind::PostDec => Some(Value::Num(oldn)),
                UpdKind::IncDiscard | UpdKind::DecDiscard => None,
            }
        }
    })
}

/// [`step_value`] pushing its result onto the VM's operand stack.
/// Where [`step_and_store`] pushes: the run loop's [`VmStack`] or a plain `Vec` (helpers
/// outside the loop).
pub(crate) trait PushValue {
    fn push_value(&mut self, v: Value);
}
impl PushValue for VmStack {
    #[inline(always)]
    fn push_value(&mut self, v: Value) {
        self.push(v)
    }
}
impl PushValue for Vec<Value> {
    #[inline(always)]
    fn push_value(&mut self, v: Value) {
        self.push(v)
    }
}

pub(crate) fn step_and_store(
    i: &mut Interp,
    stack: &mut impl PushValue,
    kind: UpdKind,
    old: Value,
    set: impl FnOnce(&mut Interp, Value) -> Result<(), Abrupt>,
) -> Result<(), Abrupt> {
    if let Some(v) = step_value(i, kind, old, set)? {
        stack.push_value(v);
    }
    Ok(())
}

/// The generic tail of the binary-operator handlers: pop both operands, run the interpreter's
/// `binary`, push the result. Out of line so the Number fast paths stay small.
#[inline(never)]
fn bin_slow(i: &mut Interp, stack: &mut VmStack, op: &'static str) -> Result<(), Abrupt> {
    let b = stack.pop_val();
    let a = stack.pop_val();
    let v = i.binary(op, a, b)?;
    stack.push(v);
    Ok(())
}

/// Number ⊕ Number in place: the result overwrites the left operand's slot (both operands are
/// Numbers, so nothing needs dropping) with one full-width store.
#[inline(always)]
fn bin_num(
    i: &mut Interp,
    stack: &mut VmStack,
    op: &'static str,
    f: impl Fn(f64, f64) -> f64,
) -> Result<(), Abrupt> {
    if let (Value::Num(x), Value::Num(y)) = (stack.peek(1), stack.peek(0)) {
        let r = f(*x, *y);
        unsafe {
            stack.forget_top(1);
            vm_regs::write_num(stack.top_ptr(0), r);
        }
        return Ok(());
    }
    bin_slow(i, stack, op)
}

#[inline(always)]
fn bin_i32(
    i: &mut Interp,
    stack: &mut VmStack,
    op: &'static str,
    f: impl Fn(i32, i32) -> i32,
) -> Result<(), Abrupt> {
    if let (Value::Num(x), Value::Num(y)) = (stack.peek(1), stack.peek(0)) {
        let r = f(crate::eval::to_int32(*x), crate::eval::to_int32(*y)) as f64;
        unsafe {
            stack.forget_top(1);
            vm_regs::write_num(stack.top_ptr(0), r);
        }
        return Ok(());
    }
    bin_slow(i, stack, op)
}

#[inline(always)]
fn bin_cmp(
    i: &mut Interp,
    stack: &mut VmStack,
    op: &'static str,
    f: impl Fn(f64, f64) -> bool,
) -> Result<(), Abrupt> {
    if let (Value::Num(x), Value::Num(y)) = (stack.peek(1), stack.peek(0)) {
        let r = f(*x, *y);
        unsafe {
            stack.forget_top(1);
            vm_regs::write_bool(stack.top_ptr(0), r);
        }
        return Ok(());
    }
    bin_slow(i, stack, op)
}

impl Chunk {
    pub(crate) fn jit_ops(&self) -> &[Op] {
        &self.ops
    }
    /// Name-cache hit check (see [`NameIc`] for the validation story): a pointer compare, a
    /// generation compare, and a value clone. `None` = miss (including TDZ — the slow path
    /// throws the proper error).
    #[inline]
    fn name_ic_hit(&self, i: &Interp, env: &Env, c: u32) -> Option<Value> {
        if let Some(value) = self.name_path_hit(i, env, c) {
            return Some(value);
        }
        let ic = self.name_caches[c as usize].get();
        let raw = Rc::as_ptr(env) as usize;
        if ic.env == raw {
            let b = env.borrow();
            if b.vars.generation() != ic.gen {
                return None;
            }
            // The unchanged generation proves the map is structurally untouched since the fill:
            // the pointer is live and the resolution unchanged (see NameIc). The value and TDZ
            // flag are read live — in-place writes flow through.
            let bd = unsafe { &*(ic.binding as usize as *const crate::interpreter::Binding) };
            return if bd.initialized {
                Some(bd.value.clone())
            } else {
                None
            };
        }
        if ic.env == raw | 1 {
            // Global-object mode (see NameIc): scope still empty of this name (generation),
            // global layout unchanged (shape) → the cached slot is still the resolution.
            if env.borrow().vars.generation() != ic.gen {
                return None;
            }
            let g = i.global.borrow();
            if !matches!(g.exotic, crate::value::Exotic::None)
                || g.props.shape() != (ic.binding >> 32) as u32
            {
                return None;
            }
            let p = g.props.entry_at(ic.binding as u32 as usize)?;
            if p.accessor() {
                return None;
            }
            return Some(p.value());
        }
        if ic.env & 2 != 0 {
            // Depth-1 mode (see NameIc): `env` is this chunk's fresh activation. Its expected
            // generation proves it holds exactly the chunk's cap_inits — which can never
            // include a LoadName'd free name — so the parent resolution still applies; the
            // parent's generation proves the binding pointer live and unmoved.
            let b = env.borrow();
            if b.vars.generation() != ic.act_gen {
                return None;
            }
            let p = b.parent.as_ref()?;
            if Rc::as_ptr(p) as usize | 2 != ic.env {
                return None;
            }
            let pb = p.borrow();
            if pb.vars.generation() != ic.gen {
                return None;
            }
            let bd = unsafe { &*(ic.binding as usize as *const crate::interpreter::Binding) };
            return if bd.initialized {
                Some(bd.value.clone())
            } else {
                None
            };
        }
        None
    }
    /// [`Chunk::name_ic_hit`] of an object value, as its `Gc::as_ptr` (no clone): the JIT's
    /// call-site guards compare it with the callee they were compiled for. `None` = a miss or
    /// any other value (the caller takes the full path).
    pub(crate) fn name_ic_obj_ptr(
        &self,
        i: &Interp,
        env: &Env,
        c: u32,
    ) -> Option<*const crate::value::ObjCell> {
        if c as usize >= self.name_caches.len() {
            return None;
        }
        if let Some(p) = self.name_path_obj_ptr(i, env, c) {
            return Some(p);
        }
        let ic = self.name_caches[c as usize].get();
        let raw = Rc::as_ptr(env) as usize;
        let bind = |bd: &crate::interpreter::Binding| match &bd.value {
            Value::Obj(o) if bd.initialized => Some(crate::value::Gc::as_ptr(o)),
            _ => None,
        };
        if ic.env == raw {
            let b = env.borrow();
            if b.vars.generation() != ic.gen {
                return None;
            }
            // SAFETY: as in `name_ic_hit` (the generation proves the pointer live).
            return bind(unsafe { &*(ic.binding as usize as *const crate::interpreter::Binding) });
        }
        if ic.env == raw | 1 {
            if env.borrow().vars.generation() != ic.gen {
                return None;
            }
            let g = i.global.borrow();
            if !matches!(g.exotic, crate::value::Exotic::None)
                || g.props.shape() != (ic.binding >> 32) as u32
            {
                return None;
            }
            return g.props.entry_at(ic.binding as u32 as usize)?.obj_ptr();
        }
        if ic.env & 2 != 0 {
            let b = env.borrow();
            if b.vars.generation() != ic.act_gen {
                return None;
            }
            let p = b.parent.as_ref()?;
            if Rc::as_ptr(p) as usize | 2 != ic.env {
                return None;
            }
            let pb = p.borrow();
            if pb.vars.generation() != ic.gen {
                return None;
            }
            // SAFETY: as in `name_ic_hit`.
            return bind(unsafe { &*(ic.binding as usize as *const crate::interpreter::Binding) });
        }
        None
    }

    /// Depth-0 cache fill: the name resolves directly in `env` as a plain initialized binding —
    /// no `with` object on the scope, no live import redirect. When `env` *is* the global scope
    /// and misses, an own data property of the ordinary global object fills the global mode
    /// instead. Returns the value on success; `None` = not cacheable at this site (the caller
    /// runs the interpreter's full walk, uncached).
    fn name_ic_fill(&self, i: &Interp, env: &Env, n: u32, c: u32) -> Option<Value> {
        {
            let b = env.borrow();
            if b.with_obj().is_some() {
                return None;
            }
            if let Some(bd) = b.vars.get(&*self.names[n as usize]) {
                if !bd.initialized || bd.import {
                    return None;
                }
                let v = bd.value.clone();
                self.name_caches[c as usize].set(NameIc {
                    env: Rc::as_ptr(env) as usize,
                    binding: bd as *const _ as usize as u64,
                    gen: b.vars.generation(),
                    act_gen: 0,
                });
                drop(b);
                // Pin the scope allocation so the raw `env` compare stays ABA-safe.
                self.name_pins.borrow_mut()[c as usize] = Some(Rc::downgrade(env));
                return Some(v);
            }
            // Depth-1 fill, ONLY for a chunk that runs under an activation: `env` is then that
            // activation — fresh pointer every call (the depth-0 mode above can never hit) but
            // chunk-determined CONTENTS, so its generation alone re-proves "this name still
            // isn't shadowed here" on any later activation. Never valid for no-activation
            // chunks: their run env is a closure-instance-specific scope whose generation says
            // nothing about which names it holds.
            if self.makes_env() {
                if let Some(p) = &b.parent {
                    let pb = p.borrow();
                    if pb.with_obj().is_none() {
                        if let Some(bd) = pb.vars.get(&*self.names[n as usize]) {
                            if bd.initialized && !bd.import {
                                let v = bd.value.clone();
                                self.name_caches[c as usize].set(NameIc {
                                    env: Rc::as_ptr(p) as usize | 2,
                                    binding: bd as *const _ as usize as u64,
                                    gen: pb.vars.generation(),
                                    act_gen: b.vars.generation(),
                                });
                                let pin = Rc::downgrade(p);
                                drop(pb);
                                drop(b);
                                // Pin the PARENT: that's the raw pointer the hit compares.
                                self.name_pins.borrow_mut()[c as usize] = Some(pin);
                                return Some(v);
                            }
                        }
                    }
                }
            }
        }
        // Global mode: only when there are no intermediate scopes whose later mutation could
        // re-route the name — i.e. the chunk runs directly under the global scope.
        if !Rc::ptr_eq(env, &i.global_env) {
            return self.name_path_fill(i, env, n, c);
        }
        if i.global_proxy.is_some() || !i.ordinary_get_ptr(Gc::as_ptr(&i.global) as usize) {
            return None;
        }
        let g = i.global.borrow();
        if !matches!(g.exotic, crate::value::Exotic::None) {
            return None;
        }
        let slot = g.props.slot_of(&self.names[n as usize])?;
        let p = g.props.entry_at(slot)?;
        if p.accessor() {
            return None;
        }
        let v = p.value();
        self.name_caches[c as usize].set(NameIc {
            env: Rc::as_ptr(env) as usize | 1,
            binding: ((g.props.shape() as u64) << 32) | slot as u64,
            gen: env.borrow().vars.generation(),
            act_gen: 0,
        });
        drop(g);
        self.name_pins.borrow_mut()[c as usize] = Some(Rc::downgrade(env));
        Some(v)
    }
    /// Cached free-name read: primary or guarded-path hit, refill, then the interpreter's
    /// full walk for dynamic resolutions, imports, TDZ and uncached global properties.
    pub(crate) fn load_name_ic(
        &self,
        i: &mut Interp,
        env: &Env,
        n: u32,
        c: u32,
    ) -> Result<Value, Abrupt> {
        if let Some(v) = self.name_ic_hit(i, env, c) {
            return Ok(v);
        }
        if let Some(v) = self.name_ic_fill(i, env, n, c) {
            return Ok(v);
        }
        i.get_var(&self.names[n as usize], env)
    }

    /// Resolve a captured binding in the current activation and cache its stable map-entry
    /// address. Structural `VarMap` mutations bump `generation`; the weak scope pin prevents an
    /// activation allocation from being recycled while its raw pointer remains cached.
    fn cap_binding_ptr(&self, env: &Env, n: u32) -> *mut crate::interpreter::Binding {
        let index = n as usize;
        let raw = Rc::as_ptr(env) as usize;
        let ic = self.cap_caches[index].get();
        {
            let b = env.borrow();
            if ic.env == raw && b.vars.generation() == ic.gen {
                return ic.binding as usize as *mut crate::interpreter::Binding;
            }
        }
        let name = &self.names[index];
        let (binding, generation) = {
            let mut b = env.borrow_mut();
            let generation = b.vars.generation();
            let binding = match self
                .activation_layout
                .as_ref()
                .and_then(|layout| layout.binding_mut(&mut b.vars, index))
            {
                Some(binding) => binding,
                None => b.vars.get_mut(name).expect("captured binding missing"),
            } as *mut crate::interpreter::Binding;
            (binding, generation)
        };
        self.cap_caches[index].set(NameIc {
            env: raw,
            binding: binding as usize as u64,
            gen: generation,
            act_gen: 0,
        });
        self.cap_pins.borrow_mut()[index] = Some(Rc::downgrade(env));
        binding
    }

    pub(crate) fn load_cap_ic(&self, i: &mut Interp, env: &Env, n: u32) -> Result<Value, Abrupt> {
        let binding = unsafe { &*self.cap_binding_ptr(env, n) };
        if !binding.initialized {
            return Err(i.throw(
                "ReferenceError",
                format!(
                    "cannot access '{}' before initialization",
                    self.names[n as usize]
                ),
            ));
        }
        Ok(binding.value.clone())
    }

    pub(crate) fn store_cap_ic(
        &self,
        i: &mut Interp,
        env: &Env,
        n: u32,
        value: Value,
        initialize: bool,
    ) -> Result<(), Abrupt> {
        let binding = unsafe { &mut *self.cap_binding_ptr(env, n) };
        if !initialize && !binding.initialized {
            return Err(i.throw(
                "ReferenceError",
                format!(
                    "cannot access '{}' before initialization",
                    self.names[n as usize]
                ),
            ));
        }
        binding.value = value;
        if initialize {
            binding.initialized = true;
        }
        Ok(())
    }

    /// Cached free-name write. A cache hit validates the same resolution proof as a read and
    /// updates a live mutable binding/plain writable global property in place. Misses perform the
    /// full PutValue operation, then side-effect-freely seed a cache when the resulting resolution
    /// is cacheable.
    fn store_name_ic(
        &self,
        i: &mut Interp,
        env: &Env,
        n: u32,
        c: u32,
        value: Value,
    ) -> Result<(), Abrupt> {
        let ic = self.name_caches[c as usize].get();
        let raw = Rc::as_ptr(env) as usize;
        let value = if ic.env == raw {
            let b = env.borrow_mut();
            if b.vars.generation() == ic.gen {
                let bd = unsafe { &mut *(ic.binding as usize as *mut crate::interpreter::Binding) };
                if bd.initialized && bd.mutable && !bd.import {
                    bd.value = value;
                    return Ok(());
                }
            }
            value
        } else if ic.env == raw | 1 {
            if env.borrow().vars.generation() == ic.gen {
                let mut g = i.global.borrow_mut();
                if matches!(g.exotic, crate::value::Exotic::None)
                    && g.props.shape() == (ic.binding >> 32) as u32
                {
                    if let Some(p) = g.props.entry_at_mut(ic.binding as u32 as usize) {
                        if !p.accessor() && p.writable() {
                            p.set_value(value);
                            return Ok(());
                        }
                    }
                }
            }
            value
        } else if ic.env & 2 != 0 {
            let parent = {
                let b = env.borrow();
                (b.vars.generation() == ic.act_gen)
                    .then(|| b.parent.clone())
                    .flatten()
            };
            if let Some(parent) = parent {
                if Rc::as_ptr(&parent) as usize | 2 == ic.env {
                    let pb = parent.borrow_mut();
                    if pb.vars.generation() == ic.gen {
                        let bd = unsafe {
                            &mut *(ic.binding as usize as *mut crate::interpreter::Binding)
                        };
                        if bd.initialized && bd.mutable && !bd.import {
                            bd.value = value;
                            return Ok(());
                        }
                    }
                }
            }
            value
        } else {
            value
        };
        let value = match self.name_path_store(i, env, c, value) {
            Ok(()) => return Ok(()),
            Err(v) => v,
        };
        i.assign_free_name(&self.names[n as usize], value, env)?;
        let _ = self.name_ic_fill(i, env, n, c);
        Ok(())
    }

    /// Where free name cache `c` resolves, for native code: `Some(false)` a scope binding,
    /// `Some(true)` a global-object property, `None` not cached.
    pub(crate) fn name_site_kind(&self, c: u32) -> Option<bool> {
        let ic = self.name_caches.get(c as usize)?.get();
        if ic.env != 0 {
            return Some(ic.env & 1 != 0);
        }
        self.name_path_kind(c)
    }

    /// The global object's entry slot free name cache `c` resolves to from `env`, while the
    /// resolution still holds and the entry is a data property.
    pub(crate) fn name_global_slot(&self, i: &Interp, env: &Env, c: u32) -> Option<usize> {
        let ic = self.name_caches.get(c as usize)?.get();
        let raw = Rc::as_ptr(env) as usize;
        if ic.env == raw | 1 {
            if env.try_borrow().ok()?.vars.generation() != ic.gen {
                return None;
            }
            let g = i.global.try_borrow().ok()?;
            if !matches!(g.exotic, crate::value::Exotic::None)
                || g.props.shape() != (ic.binding >> 32) as u32
                || g.props.entry_at(ic.binding as u32 as usize)?.accessor()
            {
                return None;
            }
            return Some(ic.binding as u32 as usize);
        }
        self.name_path_global(i, env, c)
    }
    pub(crate) fn jit_frame(&self) -> (usize, usize) {
        (self.n_params, self.n_slots)
    }

    /// Whether calls run without an activation environment (nothing captured, no lexical
    /// `this`).
    pub(crate) fn jit_no_activation(&self) -> bool {
        !self.needs_env()
    }
    /// The f64 bits of a Num const.
    pub(crate) fn jit_const_num(&self, k: u32) -> Option<u64> {
        match &self.consts[k as usize] {
            Value::Num(n) => Some(n.to_bits()),
            _ => None,
        }
    }
    /// (pops, pushes) of the op at `pc`, for the static stack-depth analysis. `None` = an op the
    /// analysis can't account for.
    pub(crate) fn jit_stack_effect(&self, pc: usize) -> Option<(usize, usize)> {
        let upd = |k: &UpdKind| match k {
            UpdKind::IncDiscard | UpdKind::DecDiscard => 0,
            _ => 1,
        };
        Some(match &self.ops[pc] {
            Op::Const(_)
            | Op::Undef
            | Op::LoadLocal(_)
            | Op::LoadCap(_)
            | Op::LoadName(..)
            | Op::LoadThis
            | Op::LoadLexicalThis
            | Op::MakeClosure(..) => (0, 1),
            Op::Dup => (1, 2),
            Op::Dup2 => (2, 4),
            Op::Pop
            | Op::StoreLocal(_)
            | Op::StoreCap(_)
            | Op::StoreCapInit(_)
            | Op::StoreName(_)
            | Op::StoreNameCached(..) => (1, 0),
            Op::UpdateLocal(_, k) | Op::UpdateCap(_, k) | Op::UpdateName(_, k) => (0, upd(k)),
            Op::UpdateNameCached(_, _, k) => (0, upd(k)),
            Op::UpdateProp(_, _, k) => (1, upd(k)),
            Op::UpdateElem(k) => (2, upd(k)),
            Op::Tdz(_) => (0, 0),
            Op::GetProp(..) => (1, 1),
            Op::GetPropThis(..) => (0, 1),
            Op::GetPropLocal(..) => (0, 1),
            Op::SetProp(..) => (2, 1),
            Op::SetPropDrop(..) => (2, 0),
            Op::SetPropThisDrop(..) => (1, 0),
            Op::SetPropLocalDrop(..) => (1, 0),
            Op::GetElem => (2, 1),
            Op::SetElem => (3, 1),
            Op::SetElemDrop => (3, 0),
            Op::AppendProp(..) => (3, 0),
            Op::DestructureGuard => (1, 1),
            Op::DestructureArr(n) => (1, *n as usize),
            Op::DeleteProp(..) => (1, 1),
            Op::DeleteElem(_) => (2, 1),
            Op::CallSpread(argc) => (*argc as usize + 1, 1),
            Op::CallSpreadThis(argc) => (*argc as usize + 2, 1),
            Op::ToStr => (1, 1),
            Op::ForInKeys => (1, 1),
            Op::ForInStepL(..) => (0, 2),
            Op::GetIter => (1, 2),
            Op::IterStepL(..) => (0, 2),
            Op::IterCloseL(_) => (0, 0),
            Op::IterAbortL(_) => (1, 0),
            Op::GetElemLocal(_) => (1, 1),
            Op::ArgsLen(..) => (0, 1),
            Op::ArgsGet(..) => (1, 1),
            Op::ApplyArgs(..) => (3, 1),
            Op::SetElemLocal(_) => (2, 1),
            Op::SetElemLocalDrop(_) => (2, 0),
            Op::ToPropKey => (2, 2),
            Op::ToPropKeyLocal(_) => (1, 1),
            Op::GetMethod(..) => (1, 2),
            Op::GetMethodElem => (2, 2),
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Lt
            | Op::Gt
            | Op::Le
            | Op::Ge
            | Op::EqEq
            | Op::NotEq
            | Op::StrictEq
            | Op::StrictNotEq
            | Op::InstanceOf(_)
            | Op::GenBin(_) => (2, 1),
            Op::Neg
            | Op::Plus
            | Op::Not
            | Op::BitNot
            | Op::Typeof
            | Op::TypeofIs(..)
            | Op::Void => (1, 1),
            Op::TypeofName(_) => (0, 1),
            Op::Jump(_) => (0, 0),
            Op::JumpIfFalse(_) => (1, 0),
            Op::JumpIfNotCmp(..) => (2, 0),
            Op::JumpIfNotCmpLL(..) | Op::JumpIfNotCmpLK(..) => (0, 0),
            Op::ArithLL(..) | Op::ArithLK(..) => (0, 0),
            Op::JumpIfFalsePeek(_) | Op::JumpIfTruePeek(_) | Op::JumpIfNotNullishPeek(_) => (1, 1),
            Op::Call(argc) => (*argc as usize + 1, 1),
            Op::LoadNameForCall(..) => (0, 2),
            Op::CallWithThis(argc) => (*argc as usize + 2, 1),
            // A generic call-out (see `Op::TailCall`'s `ONE` arm); the `Return` follows.
            Op::TailCall(argc, this) | Op::TailCallSpread(argc, this) => {
                (*argc as usize + 1 + *this as usize, 1)
            }
            Op::New(argc) => (*argc as usize + 1, 1),
            Op::MakeRegExp(..) => (0, 1),
            Op::MakeArray(n) => (*n as usize, 1),
            Op::MakeObject(_, count, _) => (*count as usize, 1),
            Op::Throw | Op::Return => (1, 0),
            Op::ReturnUndef => (0, 0),
            Op::Await => (1, 1),
            Op::PushHandler(_) | Op::PopHandler => (0, 0),
            Op::GetPrivate(_) | Op::PrivateIn(_) => (1, 1),
            Op::SetPrivate(_) => (2, 1),
            Op::GetPrivateMethod(_) => (1, 2),
            Op::UpdatePrivate(_, k) => (1, upd(k)),
            Op::NewObject | Op::SuperBase => (0, 1),
            Op::MakeClass(..) => (0, 1),
            Op::InitProp(..) | Op::CopyDataProps | Op::SetProtoLit => (2, 1),
            Op::DefineField(..) => (2, 0),
            Op::InitPropComputed(_) => (3, 1),
            Op::InitMethod(_, n, _) => (if *n == u32::MAX { 2 } else { 1 }, 1),
            Op::Nip => (2, 1),
            Op::SuperGet(_) => (1, 1),
            Op::SuperGetElem => (2, 1),
            Op::SuperMethod(..) => (1, 2),
            Op::SuperMethodElem(_) => (2, 2),
            Op::ObjRest(..) => (1, 2),
            Op::NewArrayLit => (0, 1),
            Op::ArrayAppend | Op::ArrayAppendSpread => (2, 1),
            Op::ArrayHole => (1, 1),
            Op::ArrayCbGuard(_) => (2, 3),
            Op::ArrayCbHas => (2, 1),
            Op::ArrayCbDone(_) => (0, 0),
            // A computed jump: the stack-depth analysis (and the JIT) stay off these chains.
            Op::SwitchLK(..) => return None,
            // Derived-constructor ops: `super(…)` runs as a generic op, the return exits.
            Op::SuperCtor => (0, 2),
            Op::SuperCall(n) | Op::SuperCallSpread(n) => (*n as usize + 2, 1),
            Op::DerivedReturn => (1, 0),
            // The callee from the reflection frame (JIT direct calls sync theirs first).
            Op::LoadCallee | Op::LoadNewTarget => (0, 1),
            Op::Concat(n) => (*n as usize, 1),
            // Block envs: their carrier slots are touched by these ops only (the JIT leaves
            // them in memory and runs the ops generically).
            Op::BlkNew(..) | Op::BlkDecl(..) | Op::BlkCopy(_) => (0, 0),
            Op::BlkLoad(..) => (0, 1),
            Op::BlkStore(..) | Op::BlkInit(..) => (1, 0),
            Op::BlkUpdate(_, _, k) => (0, upd(k)),
            // A prefix: runs with the closure-creating op after it (whose effect counts).
            Op::InEnv(_) => (0, 0),
            Op::NewSpread(argc) => (*argc as usize + 1, 1),
            Op::IterRestL(..) => (0, 1),
            // Generator ops stay on the interpreter.
            Op::InitialYield
            | Op::Yield
            | Op::YieldDelegate(_)
            | Op::GetAsyncIter
            | Op::AsyncIterNext(..)
            | Op::AsyncIterResult(_)
            | Op::AsyncCloseCall(..)
            | Op::AsyncCloseCheck(_)
            | Op::AsyncDelegateInit
            | Op::AsyncDelegateCall(..)
            | Op::AsyncDelegateResult(..)
            | Op::AsyncDelegateCloseReject(..)
            | Op::AsyncDelegateSpecial(..) => return None,
            Op::ImportCall(_, opts) => (1 + *opts as usize, 1),
        })
    }
}

/// A local read by a fused op still in its TDZ: the `LoadLocal` ReferenceError.
fn tdz_check(
    i: &mut Interp,
    chunk: &Chunk,
    reads: [(&Value, Option<u16>); 2],
) -> Result<(), Abrupt> {
    for (v, s) in reads {
        if let (Value::Empty, Some(s)) = (v, s) {
            return Err(i.throw(
                "ReferenceError",
                format!(
                    "cannot access '{}' before initialization",
                    chunk.slot_names[s as usize]
                ),
            ));
        }
    }
    Ok(())
}

/// `s[n]` on a string primitive known all-ASCII (byte index == code-unit index): the one-unit
/// string for an in-range integer index. `None` = generic path (out of range, non-integer,
/// possibly non-ASCII content).
#[inline]
pub(crate) fn str_index_fast(s: &crate::lstr::LStr, n: f64) -> Option<Value> {
    if !s.ascii_hint() || !(0.0..s.len() as f64).contains(&n) || n.fract() != 0.0 {
        return None;
    }
    Some(Value::Str(crate::jstr::unit_lstr(u16::from(
        s.as_bytes()[n as usize],
    ))))
}

/// `s[n]` for an in-range integer index of a string that may be non-ASCII (through the
/// UTF-16 position index). `None` = generic path.
#[inline(never)]
pub(crate) fn str_index_units(
    i: &mut crate::interpreter::Interp,
    s: &crate::lstr::LStr,
    n: f64,
) -> Option<Value> {
    if !(0.0..u32::MAX as f64).contains(&n) || n.fract() != 0.0 {
        return None;
    }
    i.unit_at(s, n as usize)
        .map(|u| Value::Str(crate::jstr::unit_lstr(u)))
}

/// `slots[d] += y` for a string accumulator (`s += x` / `s = s + x` fused into
/// `ArithLL`/`ArithLK` with `d` the left operand) and a primitive `y` whose ToPrimitive and
/// ToString run no user code. The slot's handle is taken out first, so a string referenced only
/// by this local is uniquely owned and grows in place (capacity-doubled by
/// [`crate::lstr::LStr::concat_grown`] when full) — the amortized-O(append) accumulator loop
/// instead of copying the whole accumulation per step. `false` = not applicable, nothing touched.
#[inline]
pub(crate) fn append_to_slot(slots: &mut [Value], d: usize, y: &Value) -> bool {
    let mut buf = [0u8; 32];
    let rhs: &str = match y {
        Value::Str(s) => s,
        Value::Num(n) => {
            // Integers are the common suffix (`s += i`); anything else takes the generic path.
            if n.fract() != 0.0 || n.abs() >= 1e15 || (*n == 0.0 && n.is_sign_negative()) {
                return false;
            }
            let mut v = n.abs() as u64;
            let mut at = buf.len();
            loop {
                at -= 1;
                buf[at] = b'0' + (v % 10) as u8;
                v /= 10;
                if v == 0 {
                    break;
                }
            }
            if *n < 0.0 {
                at -= 1;
                buf[at] = b'-';
            }
            std::str::from_utf8(&buf[at..]).unwrap_or("")
        }
        Value::Bool(true) => "true",
        Value::Bool(false) => "false",
        Value::Undefined => "undefined",
        Value::Null => "null",
        _ => return false,
    };
    let Value::Str(cur) = &slots[d] else {
        return false;
    };
    if size::sum(cur.len(), rhs.len(), crate::interpreter::MAX_STR_LEN).is_err()
        || crate::jstr::needs_join_fixup(cur, rhs)
    {
        return false;
    }
    let Value::Str(mut s) = std::mem::replace(&mut slots[d], Value::Undefined) else {
        unreachable!("checked above")
    };
    if !s.append_in_place(rhs) {
        s = if s.len() < 16 {
            crate::lstr::LStr::concat2(&s, rhs)
        } else {
            s.concat_grown(rhs)
        };
    }
    slots[d] = Value::Str(s);
    true
}

/// The generic path of a fused local arithmetic op: TDZ checks, then `binary`.
#[cold]
#[inline(never)]
fn arith_slow(
    i: &mut Interp,
    chunk: &Chunk,
    kind: ArithKind,
    x: Value,
    y: Value,
    a: u16,
    b: Option<u16>,
) -> Result<Value, Abrupt> {
    tdz_check(i, chunk, [(&x, Some(a)), (&y, b)])?;
    i.binary(kind.name(), x, y)
}

/// Fuse `LoadLocal LoadLocal|Const <arith> StoreLocal` into [`Op::ArithLL`]/[`Op::ArithLK`] where
/// no jump lands inside the run, then renumber every jump target.
#[cfg(feature = "compiler")]
fn peephole(ops: &mut Vec<Op>) {
    let n = ops.len();
    let mut target = vec![false; n + 1];
    for op in ops.iter() {
        if let Some(t) = crate::jit_ir::jump_target(op) {
            target[t.min(n)] = true;
        }
    }
    let mut out = Vec::with_capacity(n);
    let mut map = vec![0u32; n + 1];
    let mut pc = 0;
    while pc < n {
        map[pc] = out.len() as u32;
        // `<local> <local|const> <cmp> JumpIfFalse(t)` — the shape of `switch` case tests and
        // `if (x === K)` chains — as one compare-and-branch reading its operands in place. The
        // fused op's slow path is the same TDZ check, `binary`, ToBoolean.
        if pc + 3 < n && !target[pc + 1] && !target[pc + 2] && !target[pc + 3] {
            if let (Op::LoadLocal(a), rhs, Some(kind), Op::JumpIfFalse(t)) = (
                ops[pc],
                ops[pc + 1],
                CmpKind::of_op(&ops[pc + 2]),
                ops[pc + 3],
            ) {
                let fused = match rhs {
                    Op::LoadLocal(b) => Some(Op::JumpIfNotCmpLL(kind, a, b, t)),
                    Op::Const(k) => Some(Op::JumpIfNotCmpLK(kind, a, k, t)),
                    _ => None,
                };
                if let Some(op) = fused {
                    out.push(op);
                    pc += 4;
                    continue;
                }
            }
        }
        // `<cmp> JumpIfFalse(t)`: no Boolean pushed and popped.
        if pc + 1 < n && !target[pc + 1] {
            if let (Some(kind), Op::JumpIfFalse(t)) = (CmpKind::of_op(&ops[pc]), ops[pc + 1]) {
                out.push(Op::JumpIfNotCmp(kind, t));
                pc += 2;
                continue;
            }
        }
        if pc + 3 < n && !target[pc + 1] && !target[pc + 2] && !target[pc + 3] {
            if let (Op::LoadLocal(a), rhs, Some(kind), Op::StoreLocal(d)) = (
                ops[pc],
                ops[pc + 1],
                ArithKind::of(&ops[pc + 2]),
                ops[pc + 3],
            ) {
                let fused = match rhs {
                    Op::LoadLocal(b) => Some(Op::ArithLL(kind, d, a, b)),
                    Op::Const(k) => Some(Op::ArithLK(kind, d, a, k)),
                    _ => None,
                };
                if let Some(op) = fused {
                    out.push(op);
                    pc += 4;
                    continue;
                }
            }
        }
        out.push(ops[pc]);
        pc += 1;
    }
    map[n] = out.len() as u32;
    if out.len() == n {
        return;
    }
    remap_targets(&mut out, &map);
    *ops = out;
}

/// Rewrite every pc operand through `map` (old pc → new pc) after a pass moved ops.
#[cfg(feature = "compiler")]
fn remap_targets(ops: &mut [Op], map: &[u32]) {
    for op in ops.iter_mut() {
        match op {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfNotCmp(_, t)
            | Op::JumpIfNotCmpLL(.., t)
            | Op::JumpIfNotCmpLK(.., t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t)
            | Op::PushHandler(t) => *t = map[*t as usize],
            _ => {}
        }
    }
}

/// The generic path of a fused local compare: TDZ checks (left, then right), then `binary`.
#[cold]
#[inline(never)]
fn cmp_slow(
    i: &mut Interp,
    chunk: &Chunk,
    kind: CmpKind,
    x: Value,
    y: Value,
    a: u16,
    b: Option<u16>,
) -> Result<bool, Abrupt> {
    tdz_check(i, chunk, [(&x, Some(a)), (&y, b)])?;
    let v = i.binary(kind.name(), x, y)?;
    Ok(i.to_boolean(&v))
}

/// `===` / `!==` on the top two stack values: no coercion and no user code, so the result is
/// computed on the operands in place (no `binary` dispatch through the operator string).
#[inline(always)]
fn strict_cmp(i: &Interp, stack: &mut VmStack, negate: bool) {
    let r = match (stack.peek(1), stack.peek(0)) {
        (Value::Num(x), Value::Num(y)) => (x == y) != negate,
        (x, y) => i.strict_equals(x, y) != negate,
    };
    let b = stack.pop_val();
    vm_regs::drop_fast(b);
    let a = stack.pop_val();
    vm_regs::drop_fast(a);
    stack.push_bool(r);
}

/// Recognize `typeof x ==/===/!=/!== "<kind>"` in either operand order. The literal side has no
/// effects, so evaluating only `x` preserves order. Literals `typeof` never produces stay unfused.
fn typeof_test<'e>(
    op: &str,
    left: &'e Expr,
    right: &'e Expr,
) -> Option<(&'e Expr, TypeofKind, bool)> {
    let negated = match op {
        "==" | "===" => false,
        "!=" | "!==" => true,
        _ => return None,
    };
    let (arg, lit) = match (left, right) {
        (Expr::Unary { op: "typeof", arg }, Expr::Str(lit))
        | (Expr::Str(lit), Expr::Unary { op: "typeof", arg }) => (arg, lit),
        _ => return None,
    };
    Some((&**arg, TypeofKind::from_literal(lit)?, negated))
}
thread_local! {
    /// Every property name a chunk ever carried, kept for the thread's lifetime. The stub cache
    /// keys on the name's data pointer, and a chunk dies with its function node — a nested
    /// function's node goes when the enclosing body is released cold and no closure holds it —
    /// so a chunk-owned name could be freed and its address handed to a different name; the
    /// stub entry would then answer the new name with the old slot. Interning bounds the set by
    /// the distinct names in the program and makes the pointer identity permanent.
    static NAMES: std::cell::RefCell<crate::fasthash::FastSet<Rc<str>>> = Default::default();
}

fn intern_name(name: &str) -> Rc<str> {
    NAMES.with(|names| {
        let mut names = names.borrow_mut();
        if let Some(n) = names.get(name) {
            return n.clone();
        }
        let n: Rc<str> = Rc::from(name);
        names.insert(n.clone());
        n
    })
}

/// The hidden parameter window of a virtual `arguments` / rest object (see
/// [`Chunk::virt_base`]): calls with up to this many arguments (past the positional ones, for
/// a rest array) never build it.
const VIRT_WINDOW: usize = 8;

/// The rest-array bit of [`Op::ArgsLen`] / [`Op::ArgsGet`]'s element-base operand.
pub(crate) const VIRT_REST: u16 = 0x8000;

/// The compiler's view of a virtual `arguments` / rest object (see [`Chunk::virt_base`]).
#[derive(Clone)]
struct VirtC {
    /// The object's slot (holding the element count).
    slot: u16,
    /// The first element's slot.
    base: u16,
    rest: bool,
    /// The end of the parameter window a write to which would break the object's reads:
    /// every parameter of an `arguments` object (whose elements they are), none of a rest
    /// array's (whose elements are all hidden).
    end: u16,
    /// Used some other way than `.length` / `[k]` reads, or a parameter is written.
    escaped: bool,
}

impl VirtC {
    fn tag(&self) -> u16 {
        self.base | if self.rest { VIRT_REST } else { 0 }
    }

    fn escapes(&self, op: &Op) -> bool {
        let s = self.slot;
        if op_slots(op).contains(&s) || matches!(*op, Op::SwitchLK(t, _) if t == s) {
            return true;
        }
        if self.rest {
            return false;
        }
        let w = |t: u16| t < self.end;
        match *op {
            Op::StoreLocal(t) | Op::UpdateLocal(t, _) => w(t),
            Op::ArithLL(_, d, ..) | Op::ArithLK(_, d, ..) => w(d),
            Op::IterStepL(a, b) | Op::IterRestL(a, b) => w(a) || w(b),
            Op::ForInStepL(a, b, c) => w(a) || w(b) || w(c),
            _ => false,
        }
    }
}

/// A virtual object's real form over its `vals`: the rest array, or the (unmapped: no
/// parameter is ever written) `arguments` object of the current function frame.
fn virt_materialize(i: &mut Interp, rest: bool, vals: &[Value]) -> Value {
    if rest {
        i.make_array_iter(vals.iter().cloned())
    } else {
        jit::sync_frames(i);
        Value::Obj(i.make_compiled_arguments_unaliased(vals))
    }
}

/// The virtual object of slot `s` (see [`Op::ArgsLen`]) as a real object.
fn virt_object(i: &mut Interp, slots: &[Value], s: u16, tag: u16) -> Value {
    match &slots[s as usize] {
        Value::Num(n) => {
            let base = (tag & !VIRT_REST) as usize;
            let vals = &slots[base..base + *n as usize];
            virt_materialize(i, tag & VIRT_REST != 0, vals)
        }
        v => v.clone(),
    }
}

/// [`Op::ArgsLen`].
pub(crate) fn virt_len(i: &mut Interp, slots: &[Value], s: u16, tag: u16) -> Result<Value, Abrupt> {
    if let Value::Num(n) = slots[s as usize] {
        return Ok(Value::Num(n));
    }
    let o = virt_object(i, slots, s, tag);
    i.get_member(&o, "length")
}

/// [`Op::ArgsGet`].
pub(crate) fn virt_get(
    i: &mut Interp,
    slots: &[Value],
    s: u16,
    tag: u16,
    key: Value,
) -> Result<Value, Abrupt> {
    if let (Value::Num(n), Value::Num(k)) = (&slots[s as usize], &key) {
        if *k >= 0.0 && *k < *n && k.fract() == 0.0 {
            let base = (tag & !VIRT_REST) as usize;
            return Ok(slots[base + *k as usize].clone());
        }
    }
    let o = virt_object(i, slots, s, tag);
    let k = i.to_property_key(&key)?;
    i.get_member(&o, &k)
}

/// [`Op::ApplyArgs`] over `[f, apply, t]`.
pub(crate) fn virt_apply(
    i: &mut Interp,
    slots: &[Value],
    s: u16,
    tag: u16,
    fat: &[Value],
) -> Result<Value, Abrupt> {
    let (f, apply, t) = (&fat[0], &fat[1], &fat[2]);
    let intrinsic = match apply {
        Value::Obj(o) => o.try_borrow().is_ok_and(|b| {
            matches!(&b.call, crate::value::Callable::Native(fp)
                if *fp as usize == crate::builtins::nf_function_apply as crate::value::NativeFn as usize)
        }),
        _ => false,
    };
    if let (true, Value::Num(n)) = (intrinsic && f.is_callable(), &slots[s as usize]) {
        let base = (tag & !VIRT_REST) as usize;
        let vals: Vec<Value> = slots[base..base + *n as usize].to_vec();
        return i.call(f.clone(), t.clone(), &vals);
    }
    let o = virt_object(i, slots, s, tag);
    i.call(apply.clone(), f.clone(), &[t.clone(), o])
}

/// The pieces of a template literal's desugared `+` chain (see `Parser::build_template`): a
/// left-leaning chain whose leaves are all string literals or `ToStr` substitutions, at least
/// one of them a substitution (only template desugaring builds a `ToStr`). Every `+` in it is
/// then a string concatenation, and the pieces evaluate left to right.
#[cfg(feature = "compiler")]
fn template_chain<'a>(left: &'a Expr, right: &'a Expr) -> Option<Vec<&'a Expr>> {
    let leaf = |e: &Expr| matches!(e, Expr::Str(_) | Expr::ToStr(_));
    let mut rev = vec![right];
    let mut cur = left;
    loop {
        match cur {
            Expr::Binary {
                op: "+",
                left,
                right,
            } if leaf(right) => {
                rev.push(right);
                cur = left;
            }
            e => {
                rev.push(e);
                break;
            }
        }
    }
    if !rev.iter().all(|e| leaf(e)) || !rev.iter().any(|e| matches!(e, Expr::ToStr(_))) {
        return None;
    }
    rev.reverse();
    Some(rev)
}

/// `Op::Concat`: the Strings concatenated (a surrogate pair split across a seam is rejoined).
pub(crate) fn concat_strs_fast(parts: &[Value]) -> Option<Value> {
    let mut strs: Vec<&str> = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            Value::Str(s) => strs.push(s.as_str()),
            _ => return None,
        }
    }
    let total: usize = strs.iter().map(|s| s.len()).sum();
    if total > crate::interpreter::MAX_STR_LEN
        || strs
            .windows(2)
            .any(|w| crate::jstr::needs_join_fixup(w[0], w[1]))
    {
        return None;
    }
    Some(Value::Str(crate::lstr::LStr::concat_n(&strs)))
}

pub(crate) fn concat_strs(i: &mut Interp, parts: &[Value]) -> Result<Value, Abrupt> {
    let mut strs: Vec<&str> = Vec::with_capacity(parts.len());
    for p in parts {
        match p {
            Value::Str(s) => strs.push(s.as_str()),
            _ => return Err(i.throw("TypeError", "internal: template part is not a string")),
        }
    }
    let total: usize = strs.iter().map(|s| s.len()).sum();
    if total > crate::interpreter::MAX_STR_LEN {
        return Err(i.throw("RangeError", "Invalid string length"));
    }
    if strs
        .windows(2)
        .any(|w| crate::jstr::needs_join_fixup(w[0], w[1]))
    {
        let mut acc = String::new();
        for s in strs {
            acc = crate::jstr::concat(&acc, s);
        }
        return Ok(Value::Str(acc.into()));
    }
    Ok(Value::Str(crate::lstr::LStr::concat_n(&strs)))
}
