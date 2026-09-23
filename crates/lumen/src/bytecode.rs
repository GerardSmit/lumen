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
mod for_in;
mod name_path;
mod object_literal;
mod parameters;
mod switch;
mod this_binding;
#[cfg(test)]
mod write_strictness;

use crate::value::Gc;
use std::rc::Rc;

use crate::ast::*;
use crate::interpreter::{Abrupt, Env, Interp};
use crate::value::Value;

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
#[derive(Clone, Copy, Debug)]
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

    fn of(i: &crate::interpreter::Interp, v: &Value) -> Self {
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

#[derive(Clone, Copy, Debug)]
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
}

/// An active `try` region on the VM's handler stack.
struct Handler {
    /// Where to jump on a throw (the catch entry).
    catch_pc: usize,
    /// The operand-stack depth to unwind to before pushing the exception.
    stack_depth: usize,
}

/// How one captured binding seeds into the activation environment at entry (in order).
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
}

impl Chunk {
    /// Whether the body (or an inner arrow chain) reads `this`, so the caller must bind it.
    pub fn uses_this(&self) -> bool {
        self.uses_this || self.env_this
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

// ---------------------------------------------------------------------------------------------
// Capture analysis
// ---------------------------------------------------------------------------------------------

/// Which names the body's *inner functions* can resolve to the outer function's locals — the set
/// that must live in a real activation environment instead of VM slots. Also whether any inner
/// arrow chain reads the outer `this`.
///
/// Soundness rule: a name wrongly treated as local to an inner function would silently resolve
/// past the activation to the wrong binding, so everything not fully understood returns `None`
/// (direct eval, `with`, sloppy block function declarations, module syntax, …) and the caller
/// bails to the tree-walker.
struct CaptureScan {
    /// Declared-name scopes, innermost last, each tagged with the function-nesting depth it
    /// belongs to (0 = the function being compiled) and its push serial.
    scopes: Vec<(std::collections::HashSet<String>, u32, u32)>,
    fn_depth: u32,
    /// Names resolving from depth > 0 to a depth-0 scope.
    captured: std::collections::HashSet<String>,
    /// Names declared by a depth-0 scope that is NOT the function's top scope (block lexicals,
    /// for-head lexicals, catch params). If one of these is captured, per-block binding
    /// freshness matters — unless the name qualifies for activation homing (see
    /// `homable_inner_lets`), the caller bails.
    depth0_inner_decls: std::collections::HashSet<String>,
    /// Activation-homing candidates: a plain `let` declared by a once-per-call depth-0 scope
    /// (a block/switch outside every loop — freshness never matters) with no ENCLOSING
    /// declaration of the same name (an enclosing slot would wrongly shadow the env binding
    /// inside the block), keyed name → declaring scope serial. Same-name declarations nested
    /// INSIDE the candidate's scope are fine (their slots shadow the env binding correctly);
    /// any other same-name declaration poisons the entry. At the end a candidate homes only
    /// if every capture of the name resolved through ITS scope (see `captured_serials`) and
    /// the name was never a free/global reference.
    candidates: std::collections::HashMap<String, u32>,
    /// Names that ever entered (or were disqualified from) candidacy — a second same-name
    /// once-per-call `let` cannot home (both would map to ONE activation binding).
    ever_candidates: std::collections::HashSet<String>,
    /// For candidate names: the scope serials their captures resolved through.
    captured_serials: std::collections::HashMap<String, std::collections::HashSet<u32>>,
    /// Names referenced somewhere they did NOT resolve to a scope (free/global uses).
    free_refs: std::collections::HashSet<String>,
    /// Innermost loop nesting at the current walk position (for-head scopes and every scope
    /// pushed inside a loop body are never homable).
    loop_depth: u32,
    /// Scope-push counter (the serial stored per scope for capture attribution).
    next_serial: u32,
    /// Whether `this` is read from an inner arrow chain rooted at the outer function.
    env_this: bool,
    /// Arrow-ness of each enclosing function on the current path (index 0 = the outer function).
    arrow_path: Vec<bool>,
}

/// Collect every binding a `Pattern` introduces.
fn pat_idents(p: &Pattern, out: &mut std::collections::HashSet<String>) {
    match p {
        Pattern::Ident(n) => {
            out.insert(n.clone());
        }
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Hole => {}
                    ArrayPatElem::Elem { pattern, .. } => pat_idents(pattern, out),
                    ArrayPatElem::Rest(p) => pat_idents(p, out),
                }
            }
        }
        Pattern::Object(o) => {
            for pr in &o.props {
                pat_idents(&pr.value, out);
            }
            if let Some(r) = &o.rest {
                out.insert(r.clone());
            }
        }
        Pattern::Member(_) => {}
    }
}

/// Collect the function-scoped `var` names (and direct top-level function-declaration names) of a
/// body: recurses through blocks/loops/switch/try but never into nested functions or classes.
/// `top` distinguishes direct body statements (whose FuncDecls hoist) from block-level ones.
/// Returns false on a construct whose hoisting we don't model (sloppy Annex B block functions).
fn hoisted_vars(
    stmts: &[Stmt],
    top: bool,
    strict: bool,
    out: &mut std::collections::HashSet<String>,
) -> bool {
    for s in stmts {
        if !hoisted_vars_stmt(s, top, strict, out) {
            return false;
        }
    }
    true
}

fn hoisted_vars_stmt(
    s: &Stmt,
    top: bool,
    strict: bool,
    out: &mut std::collections::HashSet<String>,
) -> bool {
    match s {
        Stmt::VarDecl {
            kind: DeclKind::Var,
            decls,
        } => {
            for (p, _) in decls {
                pat_idents(p, out);
            }
            true
        }
        Stmt::FuncDecl(f) => {
            if top {
                if let Some(n) = &f.name {
                    out.insert(n.clone());
                }
                true
            } else {
                // Block-level function declaration: strict = block-scoped lexical (handled by the
                // block scope in the walker); sloppy = Annex B promotion we don't model — bail.
                strict
            }
        }
        Stmt::Block(b) => hoisted_vars(b, false, strict, out),
        Stmt::If { cons, alt, .. } => {
            hoisted_vars_stmt(cons, false, strict, out)
                && alt
                    .as_deref()
                    .map(|a| hoisted_vars_stmt(a, false, strict, out))
                    .unwrap_or(true)
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::Labeled { body, .. } => {
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::For { init, body, .. } => {
            if let Some(ForInit::VarDecl {
                kind: DeclKind::Var,
                decls,
            }) = init.as_deref()
            {
                for (p, _) in decls {
                    pat_idents(p, out);
                }
            }
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::ForInOf {
            decl, left, body, ..
        } => {
            if matches!(decl, Some(DeclKind::Var)) {
                pat_idents(left, out);
            }
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            hoisted_vars(block, false, strict, out)
                && handler
                    .as_ref()
                    .map(|(_, b)| hoisted_vars(b, false, strict, out))
                    .unwrap_or(true)
                && finalizer
                    .as_ref()
                    .map(|b| hoisted_vars(b, false, strict, out))
                    .unwrap_or(true)
        }
        Stmt::Switch { cases, .. } => cases
            .iter()
            .all(|c| hoisted_vars(&c.body, false, strict, out)),
        _ => true,
    }
}

impl CaptureScan {
    /// Analyze `func`, returning (captured names, inner-arrow-reads-this) or `None` to bail.
    fn run(func: &Function) -> Option<(std::collections::HashSet<String>, bool, Vec<String>)> {
        let mut sc = CaptureScan {
            scopes: Vec::new(),
            fn_depth: 0,
            captured: Default::default(),
            depth0_inner_decls: Default::default(),
            candidates: Default::default(),
            ever_candidates: Default::default(),
            captured_serials: Default::default(),
            free_refs: Default::default(),
            loop_depth: 0,
            next_serial: 0,
            env_this: false,
            arrow_path: vec![func.is_arrow],
        };
        sc.fn_body(func)?;
        // A captured name declared by an inner depth-0 scope needs per-block freshness —
        // except a candidate whose EVERY capture resolved through its own scope (a same-name
        // capture through any other binding — a for-of head, another block — captured a
        // DIFFERENT binding, which one activation slot can't express), which the compiler
        // homes activation-wide instead.
        let mut homed: Vec<String> = Vec::new();
        for n in &sc.captured {
            if sc.depth0_inner_decls.contains(n) {
                let ok = sc.candidates.get(n).is_some_and(|cs| {
                    !sc.free_refs.contains(n)
                        && sc
                            .captured_serials
                            .get(n)
                            .is_some_and(|set| set.len() == 1 && set.contains(cs))
                });
                if !ok {
                    return None;
                }
                homed.push(n.clone());
            }
        }
        // Homed names leave `captured`: the remaining consumers (param/var/body-lexical
        // homing, the for-of gates) concern OTHER bindings of the name, and any same-name
        // binding that could conflict already poisoned candidacy above.
        for n in &homed {
            sc.captured.remove(n);
        }
        homed.sort(); // deterministic cap_init order
        Some((sc.captured, sc.env_this, homed))
    }

    fn push_scope(&mut self, names: std::collections::HashSet<String>) {
        self.push_scope_lets(names, Default::default());
    }

    /// Like [`CaptureScan::push_scope`]; `lets` is the subset of `names` declared by plain
    /// `let`s, which qualify for activation homing when the scope runs at most once per call
    /// (outside every loop) and the name is unique/unambiguous (see `homable_inner_lets`).
    fn push_scope_lets(
        &mut self,
        names: std::collections::HashSet<String>,
        lets: std::collections::HashSet<String>,
    ) {
        let serial = self.next_serial;
        self.next_serial += 1;
        if self.fn_depth == 0 && !self.scopes.is_empty() {
            for n in &names {
                self.depth0_inner_decls.insert(n.clone());
                let enclosed = self.scopes.iter().any(|(s, _, _)| s.contains(n));
                if self.loop_depth == 0
                    && lets.contains(n)
                    && !enclosed
                    && self.ever_candidates.insert(n.clone())
                {
                    self.candidates.insert(n.clone(), serial);
                } else {
                    // A non-qualifying declaration doesn't poison an existing candidate: a
                    // later same-name SLOT declaration (nested or sibling) shadows the env
                    // binding correctly, and a capture through it fails the serial check in
                    // `run`. It does block FUTURE candidacy — the pending-consumption scheme
                    // in the compiler requires the candidate to be the walk-order-FIRST
                    // block-lexical declaration of its name.
                    self.ever_candidates.insert(n.clone());
                }
            }
        } else if self.fn_depth == 0 {
            // The function's top scope: params/vars/body lexicals block all same-name
            // candidacy (they'd be enclosing declarations).
            for n in &names {
                self.ever_candidates.insert(n.clone());
            }
        }
        self.scopes.push((names, self.fn_depth, serial));
    }

    /// Walk a whole function: params + hoisted vars + top-level lexicals in one scope, then body.
    fn fn_body(&mut self, func: &Function) -> Option<()> {
        let mut names = std::collections::HashSet::new();
        for p in &func.params {
            pat_idents(&p.pattern, &mut names);
        }
        if !func.is_arrow {
            names.insert("arguments".to_string());
        }
        if func.is_fn_expr {
            if let Some(n) = &func.name {
                names.insert(n.clone());
            }
        }
        let body = func.body();
        if !hoisted_vars(&body, true, func.is_strict, &mut names) {
            return None;
        }
        self.declare_lexicals(&body, &mut names);
        self.push_scope(names);
        // Parameter defaults evaluate in the function scope.
        for p in &func.params {
            if let Some(d) = &p.default {
                self.expr(d)?;
            }
        }
        for s in body.iter() {
            self.stmt(s)?;
        }
        self.scopes.pop();
        Some(())
    }

    /// Add a statement list's block-scoped declarations (let/const/class, strict block functions).
    /// `lets` (when wanted) additionally collects the plain-`let` names — the only kind that
    /// qualifies for activation homing when captured (see `homable_inner_lets`).
    fn declare_lexicals(&self, stmts: &[Stmt], out: &mut std::collections::HashSet<String>) {
        self.declare_lexicals_lets(stmts, out, &mut Default::default());
    }

    fn declare_lexicals_lets(
        &self,
        stmts: &[Stmt],
        out: &mut std::collections::HashSet<String>,
        lets: &mut std::collections::HashSet<String>,
    ) {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                    decls,
                } => {
                    if matches!(
                        s,
                        Stmt::VarDecl {
                            kind: DeclKind::Let,
                            ..
                        }
                    ) {
                        for (p, _) in decls {
                            pat_idents(p, lets);
                        }
                    }
                    for (p, _) in decls {
                        pat_idents(p, out);
                    }
                }
                Stmt::ClassDecl(c) => {
                    if let Some(n) = &c.name {
                        out.insert(n.clone());
                    }
                }
                Stmt::FuncDecl(f) => {
                    // Only reached for *block-level* declarations (top-level ones are in the
                    // hoisted set); strict mode makes them block lexicals. (Sloppy already bailed
                    // in hoisted_vars.)
                    if let Some(n) = &f.name {
                        out.insert(n.clone());
                    }
                }
                _ => {}
            }
        }
    }

    fn block(&mut self, stmts: &[Stmt]) -> Option<()> {
        let mut names = std::collections::HashSet::new();
        let mut lets = std::collections::HashSet::new();
        self.declare_lexicals_lets(stmts, &mut names, &mut lets);
        self.push_scope_lets(names, lets);
        for s in stmts {
            self.stmt(s)?;
        }
        self.scopes.pop();
        Some(())
    }

    fn reference(&mut self, name: &str) {
        for (scope, depth, serial) in self.scopes.iter().rev() {
            if scope.contains(name) {
                if *depth == 0 && self.fn_depth > 0 {
                    self.captured.insert(name.to_string());
                    self.captured_serials
                        .entry(name.to_string())
                        .or_default()
                        .insert(*serial);
                }
                return;
            }
        }
        // Unresolved: a global/free name of the whole compilation — nothing to capture, but
        // it poisons activation homing for a like-named block lexical (whose env binding
        // would wrongly shadow the global for this reference).
        self.free_refs.insert(name.to_string());
    }

    /// Walk a pattern in *assignment* position (destructuring assignment): idents are references.
    fn pat_targets(&mut self, p: &Pattern) -> Option<()> {
        match p {
            Pattern::Ident(n) => {
                self.reference(n);
                Some(())
            }
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pat_targets(pattern)?;
                            if let Some(d) = default {
                                self.expr(d)?;
                            }
                        }
                        ArrayPatElem::Rest(p) => self.pat_targets(p)?,
                    }
                }
                Some(())
            }
            Pattern::Object(o) => {
                for pr in &o.props {
                    if let PropKey::Computed(k) = &pr.key {
                        self.expr(k)?;
                    }
                    self.pat_targets(&pr.value)?;
                    if let Some(d) = &pr.default {
                        self.expr(d)?;
                    }
                }
                if let Some(r) = &o.rest {
                    self.reference(r);
                }
                Some(())
            }
            Pattern::Member(e) => self.expr(e),
        }
    }

    /// Walk the expressions inside a *declaration* pattern (defaults, computed keys); the idents
    /// themselves were declared by the enclosing scope construction.
    fn pat_decl_exprs(&mut self, p: &Pattern) -> Option<()> {
        match p {
            Pattern::Ident(_) => Some(()),
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pat_decl_exprs(pattern)?;
                            if let Some(d) = default {
                                self.expr(d)?;
                            }
                        }
                        ArrayPatElem::Rest(p) => self.pat_decl_exprs(p)?,
                    }
                }
                Some(())
            }
            Pattern::Object(o) => {
                for pr in &o.props {
                    if let PropKey::Computed(k) = &pr.key {
                        self.expr(k)?;
                    }
                    self.pat_decl_exprs(&pr.value)?;
                    if let Some(d) = &pr.default {
                        self.expr(d)?;
                    }
                }
                Some(())
            }
            Pattern::Member(e) => self.expr(e),
        }
    }

    fn stmt(&mut self, s: &Stmt) -> Option<()> {
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) => self.expr(e),
            Stmt::VarDecl { decls, .. } => {
                for (p, init) in decls {
                    self.pat_decl_exprs(p)?;
                    if let Some(e) = init {
                        self.expr(e)?;
                    }
                }
                Some(())
            }
            Stmt::FuncDecl(f) => self.inner_fn(f),
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.expr(e)?;
                }
                Some(())
            }
            Stmt::If { test, cons, alt } => {
                self.expr(test)?;
                self.stmt(cons)?;
                if let Some(a) = alt {
                    self.stmt(a)?;
                }
                Some(())
            }
            Stmt::Block(b) => self.block(b),
            Stmt::While { test, body } => {
                self.expr(test)?;
                self.loop_depth += 1;
                let r = self.stmt(body);
                self.loop_depth -= 1;
                r
            }
            Stmt::DoWhile { body, test } => {
                self.loop_depth += 1;
                let r = self.stmt(body);
                self.loop_depth -= 1;
                r?;
                self.expr(test)
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                let mut names = std::collections::HashSet::new();
                if let Some(ForInit::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const,
                    decls,
                }) = init.as_deref()
                {
                    for (p, _) in decls {
                        pat_idents(p, &mut names);
                    }
                }
                // The head scope itself counts as in-loop: its lexicals are per-iteration
                // fresh, never once-per-call.
                self.loop_depth += 1;
                self.push_scope(names);
                let r = (|| {
                    match init.as_deref() {
                        Some(ForInit::VarDecl { decls, .. }) => {
                            for (p, e) in decls {
                                self.pat_decl_exprs(p)?;
                                if let Some(e) = e {
                                    self.expr(e)?;
                                }
                            }
                        }
                        Some(ForInit::Expr(e)) => self.expr(e)?,
                        None => {}
                    }
                    if let Some(t) = test {
                        self.expr(t)?;
                    }
                    if let Some(u) = update {
                        self.expr(u)?;
                    }
                    self.stmt(body)
                })();
                self.scopes.pop();
                self.loop_depth -= 1;
                r
            }
            Stmt::ForInOf {
                decl,
                left,
                right,
                body,
                ..
            } => {
                self.expr(right)?;
                let mut names = std::collections::HashSet::new();
                match decl {
                    Some(
                        DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                    ) => {
                        pat_idents(left, &mut names);
                    }
                    Some(DeclKind::Var) => {} // already in the hoisted set
                    None => {}
                }
                self.loop_depth += 1;
                self.push_scope(names);
                let r = (|| {
                    if decl.is_none() {
                        self.pat_targets(left)?;
                    } else {
                        self.pat_decl_exprs(left)?;
                    }
                    self.stmt(body)
                })();
                self.scopes.pop();
                self.loop_depth -= 1;
                r
            }
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty | Stmt::Debugger => Some(()),
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                self.block(block)?;
                if let Some((param, body)) = handler {
                    let mut names = std::collections::HashSet::new();
                    if let Some(p) = param {
                        pat_idents(p, &mut names);
                    }
                    self.declare_lexicals(body, &mut names);
                    self.push_scope(names);
                    let r = (|| {
                        if let Some(p) = param {
                            self.pat_decl_exprs(p)?;
                        }
                        for s in body {
                            self.stmt(s)?;
                        }
                        Some(())
                    })();
                    self.scopes.pop();
                    r?;
                }
                if let Some(f) = finalizer {
                    self.block(f)?;
                }
                Some(())
            }
            Stmt::Switch { disc, cases } => {
                self.expr(disc)?;
                let mut names = std::collections::HashSet::new();
                let mut lets = std::collections::HashSet::new();
                for c in cases {
                    self.declare_lexicals_lets(&c.body, &mut names, &mut lets);
                }
                self.push_scope_lets(names, lets);
                let r = (|| {
                    for c in cases {
                        if let Some(t) = &c.test {
                            self.expr(t)?;
                        }
                        for s in &c.body {
                            self.stmt(s)?;
                        }
                    }
                    Some(())
                })();
                self.scopes.pop();
                r
            }
            Stmt::Labeled { body, .. } => self.stmt(body),
            Stmt::ClassDecl(c) => self.class(c),
            // `with`, modules, and anything else unrecognized: unanalyzable.
            _ => None,
        }
    }

    /// Enter an inner function (declaration, expression, method, accessor…).
    fn inner_fn(&mut self, f: &Function) -> Option<()> {
        // Capture analysis needs the inner body; one that does not parse makes the outer
        // function uncompilable (the tree-walker reports the error when the inner is called).
        // A body parsed only for this scan is released again: compiling one enclosing function
        // must not materialise the AST of every function nested under it (V8's preparser scans
        // inner functions for free variables without keeping their trees either).
        let borrowed = f.parsed_body().is_none();
        f.ensure_body().ok()?;
        self.fn_depth += 1;
        self.arrow_path.push(f.is_arrow);
        let r = self.fn_body(f);
        self.arrow_path.pop();
        self.fn_depth -= 1;
        if borrowed {
            f.release_body();
        }
        r
    }

    fn class(&mut self, c: &Class) -> Option<()> {
        // Heritage, decorators, and computed keys evaluate at definition time (current depth);
        // method bodies / field initializers / static blocks run later (inner-function depth).
        for d in &c.decorators {
            self.expr(d)?;
        }
        if let Some(sc) = &c.superclass {
            self.expr(sc)?;
        }
        let mut names = std::collections::HashSet::new();
        if let Some(n) = &c.name {
            names.insert(n.clone());
        }
        self.push_scope(names);
        let r = (|| {
            for m in &c.members {
                for d in &m.decorators {
                    self.expr(d)?;
                }
                if let PropKey::Computed(k) = &m.key {
                    self.expr(k)?;
                }
                if let Some(f) = &m.func {
                    self.inner_fn(f)?;
                }
                if let Some(v) = &m.value {
                    // Field initializers run in an implicit method with its own `this` (the
                    // instance) — inner depth, and NOT part of any outer arrow chain.
                    self.fn_depth += 1;
                    self.arrow_path.push(false);
                    let r = self.expr(v);
                    self.arrow_path.pop();
                    self.fn_depth -= 1;
                    r?;
                }
            }
            Some(())
        })();
        self.scopes.pop();
        r
    }

    fn expr(&mut self, e: &Expr) -> Option<()> {
        match e {
            Expr::Num(_)
            | Expr::BigInt(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Undefined
            | Expr::Regex { .. }
            | Expr::Super
            | Expr::NewTarget
            | Expr::ImportMeta => Some(()),
            Expr::This => {
                // `this` read through an unbroken arrow chain from the outer function observes
                // the outer `this` — the activation must carry it.
                if self.fn_depth > 0 && self.arrow_path[1..].iter().all(|a| *a) {
                    self.env_this = true;
                }
                Some(())
            }
            Expr::Ident(n) => {
                self.reference(n);
                Some(())
            }
            Expr::Paren(i) | Expr::ToStr(i) | Expr::Await(i) | Expr::OptionalChain(i) => {
                self.expr(i)
            }
            Expr::Array(elems) => {
                for el in elems {
                    match el {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::Object(props) => {
                for p in props {
                    match p {
                        PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                            if let PropKey::Computed(k) = key {
                                self.expr(k)?;
                            }
                            self.expr(value)?;
                        }
                        PropDef::Method { key, func }
                        | PropDef::Getter { key, func }
                        | PropDef::Setter { key, func } => {
                            if let PropKey::Computed(k) = key {
                                self.expr(k)?;
                            }
                            self.inner_fn(func)?;
                        }
                        PropDef::Spread(e) | PropDef::Proto(e) => self.expr(e)?,
                    }
                }
                Some(())
            }
            Expr::Func(f) => self.inner_fn(f),
            Expr::Class(c) => self.class(c),
            Expr::Yield { arg, .. } => {
                if let Some(a) = arg {
                    self.expr(a)?;
                }
                Some(())
            }
            Expr::Unary { arg, .. } | Expr::Update { arg, .. } => self.expr(arg),
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left)?;
                self.expr(right)
            }
            Expr::Assign { target, value, .. } => {
                // A destructuring assignment target is a pattern of references.
                match &**target {
                    Expr::Array(_) | Expr::Object(_) => {
                        // Reinterpreting the literal as a pattern is the parser's job; walking it
                        // as an expression visits the same identifiers (Cover handles defaults).
                        self.expr(target)?;
                    }
                    t => self.expr(t)?,
                }
                self.expr(value)
            }
            Expr::Cond { test, cons, alt } => {
                self.expr(test)?;
                self.expr(cons)?;
                self.expr(alt)
            }
            Expr::Call { callee, args, .. } => {
                // Direct eval inside any nested function could name arbitrary outer locals.
                if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    return None;
                }
                self.expr(callee)?;
                for a in args {
                    match a {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::New { callee, args } => {
                self.expr(callee)?;
                for a in args {
                    match a {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::Member { obj, .. } => self.expr(obj),
            Expr::Index { obj, index, .. } => {
                self.expr(obj)?;
                self.expr(index)
            }
            Expr::Seq(es) => {
                for e in es {
                    self.expr(e)?;
                }
                Some(())
            }
            Expr::TaggedTemplate { tag, subs, .. } => {
                self.expr(tag)?;
                for s in subs {
                    self.expr(s)?;
                }
                Some(())
            }
            Expr::PrivateIn { obj, .. } => self.expr(obj),
            Expr::ImportCall { spec, options, .. } => {
                self.expr(spec)?;
                if let Some(o) = options {
                    self.expr(o)?;
                }
                Some(())
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------------------------

/// Compile `func` whole, or `None` if it uses anything outside the v0 subset.
pub fn compile(func: &Function) -> Option<Rc<Chunk>> {
    if func.ensure_body().is_err() {
        return None;
    }
    // Body facts the scanner already knows: `new.target` is an observation channel into the
    // activation that slots do not provide; `arguments` in an arrow is a free variable
    // we do not model. Parameterless synchronous ordinary functions can materialize an unmapped
    // arguments object into a dedicated slot (the common variadic-helper shape).
    let scan = func.scan_flags();
    if scan & SCAN_NEW_TARGET != 0 {
        log_bail("fn", "new.target");
        return None;
    }
    let uses_arguments = scan & SCAN_ARGUMENTS != 0;
    if uses_arguments && (func.is_arrow || func.is_async || !func.params.is_empty()) {
        log_bail("fn", "arguments with arrow/async/parameters");
        return None;
    }
    // Generators still run in the tree-walker (their `.return()`/`.throw()` injection and yield*
    // delegation are not modeled here). Async functions compile: `await` lowers to `Op::Await`,
    // which suspends the `VmCoro` that drives this body.
    if func.is_generator {
        log_bail("fn", "generator");
        return None;
    }
    // A named function expression binds its own name inside the body — an env-side binding the
    // slot model doesn't carry.
    if func.is_fn_expr && func.name.is_some() {
        log_bail("fn", "named function expression");
        return None;
    }
    // Capture analysis: which locals inner functions can name (they live in a real activation
    // env), and whether an inner arrow chain reads `this`. `None` = unanalyzable — bail.
    let Some((captured, env_this, block_lets)) = CaptureScan::run(func) else {
        let head: String = func
            .source()
            .as_deref()
            .unwrap_or("<no source>")
            .chars()
            .take(90)
            .collect();
        log_bail(
            "capture-scan",
            &format!(
                "unanalyzable body (eval/with/annexB/pattern) in: {}",
                head.replace('\n', " ")
            ),
        );
        return None;
    };

    let mut c = Compiler {
        // Arrows forward the enclosing binding through their scope chain. They must not
        // synthesize a new this binding for nested arrows, especially before super().
        env_this: env_this && !func.is_arrow,
        lexical_this: func.is_arrow,
        strict: func.is_strict,
        ..Compiler::default()
    };
    if uses_arguments {
        let slot = c.fresh_slot("arguments");
        c.scope_bind("arguments", slot, false);
        c.arguments_slot = Some(slot);
    }
    // Captured once-per-call block `let`s home in the activation (TDZ from entry, initialized
    // by the declaring block's own StoreCapInit); CaptureScan proved no enclosing same-name
    // declaration and block-resolved references only, so the function-flat env map is
    // faithful (nested same-name declarations shadow it through their slots).
    for name in &block_lets {
        c.cap_inits
            .push(CapInit::Lexical(Rc::from(name.as_str()), false));
        c.env_bind(name, false);
        c.homed_lets.insert(name.clone());
        c.homed_pending.insert(name.clone());
    }
    // Parameters: plain identifiers only, one positional slot each (a sloppy duplicate name
    // resolves to the later parameter, matching the env behavior where the later insert wins).
    // A captured parameter keeps its positional slot (dead) but homes in the activation env.
    let mut defaulted: Vec<(u16, &Expr, Option<u32>)> = Vec::new();
    for (k, p) in func.params.iter().enumerate() {
        if p.rest {
            log_bail("params", "rest parameter");
            return None;
        }
        let Pattern::Ident(name) = &p.pattern else {
            log_bail("params", "destructuring parameter");
            return None;
        };
        if let Some(d) = &p.default {
            // Captured defaults need a bounded initialization proof; uncaptured defaults
            // retain the existing expression-safety check below.
            if captured.contains(name) && !parameters::captured_default_safe(func, name, d) {
                log_bail("params", "unsafe captured defaulted parameter");
                return None;
            }
            let banned: std::collections::HashSet<&str> = func.params[k..]
                .iter()
                .filter_map(|q| match &q.pattern {
                    Pattern::Ident(n) => Some(n.as_str()),
                    _ => None,
                })
                .collect();
            if !parameters::default_expr_safe(d, &banned) {
                log_bail("params", "unsafe default expression");
                return None;
            }
            let cap = captured.contains(name).then(|| c.name_idx(name));
            defaulted.push((k as u16, d, cap));
        }
        let slot = c.fresh_slot(name);
        if captured.contains(name) {
            c.cap_inits
                .push(CapInit::Param(k as u16, Rc::from(name.as_str())));
            c.env_bind(name, false);
        } else {
            c.scope_bind(name, slot, false);
        }
    }
    c.n_params = func.params.len();
    // Parameter defaults fill missing/undefined arguments before anything else runs (spec
    // order: parameter binding precedes var/function hoisting).
    for (slot, d, cap) in defaulted {
        c.parameter_default(slot, d, cap).ok()?;
    }
    // Function-scoped `var`s and hoisted function declarations from the shared hoist plan.
    let body = func.body();
    for op in crate::interpreter::collect_hoist_ops(&body, func.is_strict, &[]) {
        match op {
            HoistOp::Var(name) => {
                if captured.contains(&name) {
                    if !c.env_has(&name) {
                        c.cap_inits.push(CapInit::Var(Rc::from(name.as_str())));
                        c.env_bind(&name, false);
                    }
                } else if c.lookup(&name).is_none() {
                    let slot = c.fresh_slot(&name);
                    c.scope_bind(&name, slot, false);
                }
            }
            HoistOp::VarForce(name) => {
                if captured.contains(&name) {
                    return None; // for-head reset of a captured param — stay in the oracle
                }
                let slot = match c.lookup(&name) {
                    Some((s, _)) => s,
                    None => {
                        let s = c.fresh_slot(&name);
                        c.scope_bind(&name, s, false);
                        s
                    }
                };
                if (slot as usize) < func.params.len() {
                    if func.params[slot as usize].default.is_some() {
                        return None; // reset would clobber the default (oracle order differs)
                    }
                    c.var_force_resets.push(slot);
                }
            }
            HoistOp::Fn(name, f) => {
                let fidx = c.funcs.len() as u16;
                c.funcs.push(f.clone());
                if captured.contains(&name) {
                    c.cap_inits.push(CapInit::Fn(fidx, Rc::from(name.as_str())));
                    c.env_bind(&name, false);
                } else {
                    let slot = match c.lookup(&name) {
                        Some((s, _)) => s,
                        None => {
                            let s = c.fresh_slot(&name);
                            c.scope_bind(&name, s, false);
                            s
                        }
                    };
                    // Created at entry, in hoist order, closing over the activation.
                    c.emit(Op::MakeClosure(fidx as u32, u32::MAX));
                    c.emit(Op::StoreLocal(slot));
                }
            }
            // Annex B promotions have declaration-time sync the VM doesn't model — bail.
            HoistOp::AnnexB(..) => return None,
        }
    }
    // Body-level lexicals: captured ones home in the activation (inserted in TDZ by
    // make_run_env), the rest get TDZ slots.
    if c.declare_body_lexicals(&body, &captured).is_err() {
        log_bail("body-lexicals", "unsupported declaration form");
        return None;
    }
    for stmt in body.iter() {
        if c.stmt(stmt).is_err() {
            log_bail_node("stmt-in", stmt, 80);
            return None;
        }
    }
    c.emit(Op::ReturnUndef);
    peephole(&mut c.ops);
    static DUMP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *DUMP.get_or_init(|| std::env::var_os("LUMEN_BC_DUMP").is_some()) {
        eprintln!("[bc] {:?}", func.name);
        for (pc, op) in c.ops.iter().enumerate() {
            eprintln!("  {pc:4} {op:?}");
        }
    }
    let cap_cache_len = c.names.len();
    let activation_layout = activation::ActivationLayout::new(&c.cap_inits, c.env_this, &c.names);
    Some(Rc::new(Chunk {
        ops: c.ops,
        consts: c.consts,
        names: c.names,
        n_slots: c.slot_names.len(),
        slot_names: c.slot_names,
        n_params: c.n_params,
        arguments_slot: c.arguments_slot,
        var_force_resets: c.var_force_resets,
        uses_this: c.uses_this,
        funcs: c.funcs,
        cap_inits: c.cap_inits,
        activation_layout,
        env_this: c.env_this,
        obj_maps: (0..c.obj_maps)
            .map(|_| std::cell::OnceCell::new())
            .collect(),
        caches: c.caches,
        name_pins: std::cell::RefCell::new(vec![None; c.name_caches.len()]),
        name_paths: (0..c.name_caches.len())
            .map(|_| std::cell::RefCell::new(None))
            .collect(),
        name_caches: c.name_caches,
        cap_caches: vec![std::cell::Cell::new(NameIc::EMPTY); cap_cache_len],
        cap_pins: std::cell::RefCell::new(vec![None; cap_cache_len]),
    }))
}

#[derive(Default)]
struct Compiler {
    /// Root arrow bodies read this from the closure environment.
    lexical_this: bool,
    /// The compiled function's strictness (carried into ops whose runtime behavior forks on it).
    strict: bool,
    /// Captured once-per-call block `let`s homed in the activation (see CaptureScan's
    /// `candidates`). `homed_pending` holds the ones whose declaring block hasn't been reached
    /// yet: the FIRST block-level declaration of the name consumes it (skipping slot creation);
    /// any later same-name declaration is a nested shadow and binds a slot normally.
    homed_lets: std::collections::HashSet<String>,
    homed_pending: std::collections::HashSet<String>,
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    /// Lexical scopes for slot resolution: (name, slot, is_const), innermost last.
    scopes: Vec<Vec<(String, u16, bool)>>,
    slot_names: Vec<Rc<str>>,
    n_params: usize,
    arguments_slot: Option<u16>,
    var_force_resets: Vec<u16>,
    loops: Vec<LoopCtx>,
    /// Labels collected from an enclosing `Stmt::Labeled` chain, waiting to be attached to the next
    /// loop's `LoopCtx` (drained when that loop pushes its context).
    pending_labels: Vec<String>,
    uses_this: bool,
    /// Count of object-literal template sites handed out (see `Chunk::obj_maps`).
    obj_maps: u32,
    caches: Vec<std::cell::Cell<IcState>>,
    name_caches: Vec<std::cell::Cell<NameIc>>,
    /// Number of `PushHandler` regions active at the current emission point. `break`/`continue`
    /// jumping out of a `try` block (or a for-of body, which wraps itself in a handler) must
    /// emit a `PopHandler` per region crossed, or the stale handler catches unrelated throws
    /// later in the frame.
    try_depth: u32,
    /// Slots that ever enter a temporal dead zone (an `Op::Tdz` was emitted for them). The fused
    /// element ops defer the base-slot read past key/value evaluation, which is only
    /// order-unobservable when the base can never TDZ-throw — params and `var`s qualify.
    tdz_slots: std::collections::HashSet<u16>,
    /// Captured (env-homed) function-scope-wide names → is_const. Slot scopes shadow these.
    env_names: std::collections::HashMap<String, bool>,
    funcs: Vec<Rc<Function>>,
    cap_inits: Vec<CapInit>,
    env_this: bool,
}

/// Where a name resolves inside the compiled body.
enum Home {
    Slot(u16, bool),
    /// Captured: lives in the activation env; bool = is_const.
    Env(bool),
}

#[derive(Default)]
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    /// `Compiler::try_depth` when this context was entered — the reference point for how many
    /// handler regions a `break`/`continue` targeting this context crosses.
    entry_try_depth: u32,
    /// A for-of loop's iterator slot: crossing `break`s close it (`IterCloseL`); its own
    /// `continue`s don't (the loop keeps iterating).
    foreach_iter: Option<u16>,
    /// For a for-of context: `try_depth` just after its per-iteration body handler pushed —
    /// exits emitted inside the body pop down to here before touching the handler itself.
    body_try_depth: u32,
    /// Labels naming this loop (usually zero or one; `a: b: for(…)` stacks several). A labelled
    /// `break`/`continue` searches the loop stack for the ctx carrying its target label.
    labels: Vec<String>,
    /// A `switch` context: an unlabelled `break` targets it, but `continue` skips past it to the
    /// innermost enclosing loop.
    is_switch: bool,
}

/// Debug (`LUMEN_TIER_LOG=1`): report the AST construct a compile bail came from.
fn log_bail(what: &str, detail: &str) {
    if bail_log_enabled() {
        eprintln!("[tier] unsupported {what}: {detail}");
    }
}

fn bail_log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_TIER_LOG").is_some())
}

/// Debug-formats an AST node for a bail log line, only when logging is on: a class expression's
/// Debug output walks every member, and doing that for every bail is measurable.
fn log_bail_node(what: &str, node: &dyn std::fmt::Debug, width: usize) {
    if bail_log_enabled() {
        log_bail(what, &format!("{:.width$}", format!("{node:?}")));
    }
}

/// Compilation bail: the construct is outside the v0 subset.
struct Bail;
type CResult = Result<(), Bail>;

/// Whether `e` provably cannot reassign the local `name` (for fused element ops, which defer the
/// base-slot read past this expression's evaluation). Whitelist recursion: any variant not
/// explicitly handled answers `false` (don't fuse). Calls and nested functions are safe — a slot
/// local is unobservable outside its function (that is what makes slot storage sound), so only a
/// syntactic assignment/update in this very expression could touch it.
fn no_assign_to(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Ident(_)
        | Expr::This
        | Expr::Regex { .. }
        | Expr::Func(_) => true,
        Expr::Paren(x) | Expr::ToStr(x) | Expr::Unary { arg: x, .. } => no_assign_to(x, name),
        Expr::Update { arg, .. } => match &**arg {
            Expr::Ident(n) => n != name,
            Expr::Member { obj, .. } => no_assign_to(obj, name),
            Expr::Index { obj, index, .. } => no_assign_to(obj, name) && no_assign_to(index, name),
            _ => false,
        },
        Expr::Assign { target, value, .. } => {
            let target_ok = match &**target {
                Expr::Ident(n) => n != name,
                Expr::Member { obj, .. } => no_assign_to(obj, name),
                Expr::Index { obj, index, .. } => {
                    no_assign_to(obj, name) && no_assign_to(index, name)
                }
                _ => false, // destructuring pattern — could bind `name`
            };
            target_ok && no_assign_to(value, name)
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            no_assign_to(left, name) && no_assign_to(right, name)
        }
        Expr::Cond { test, cons, alt } => {
            no_assign_to(test, name) && no_assign_to(cons, name) && no_assign_to(alt, name)
        }
        Expr::Member { obj, .. } => no_assign_to(obj, name),
        Expr::Index { obj, index, .. } => no_assign_to(obj, name) && no_assign_to(index, name),
        Expr::Call { callee, args, .. } | Expr::New { callee, args } => {
            no_assign_to(callee, name)
                && args.iter().all(|a| match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => no_assign_to(e, name),
                    ArrayElem::Hole => true,
                })
        }
        Expr::Array(elems) => elems.iter().all(|a| match a {
            ArrayElem::Item(e) | ArrayElem::Spread(e) => no_assign_to(e, name),
            ArrayElem::Hole => true,
        }),
        _ => false,
    }
}

impl Compiler {
    fn emit(&mut self, op: Op) -> usize {
        self.ops.push(op);
        self.ops.len() - 1
    }
    /// Reserve a fresh inline-cache slot (starts empty) for a property-access op.
    fn new_cache(&mut self) -> u32 {
        // PROP_IC_WAYS consecutive ways per site: consumers address way 1; probes reach the
        // others at `cache_ptr + k` (see `Interp::ic_way`). Keeps every existing call site
        // untouched.
        let idx = self.caches.len() as u32;
        for _ in 0..PROP_IC_WAYS {
            self.caches.push(std::cell::Cell::new(IcState::EMPTY));
        }
        idx
    }
    /// Reserve one cache cell for an op whose generated template has a single stable shape.
    /// Unlike property sites, `instanceof` does not need four polymorphic ways; keeping this
    /// separate avoids paying 96 bytes per source occurrence.
    fn new_single_cache(&mut self) -> u32 {
        let idx = self.caches.len() as u32;
        self.caches.push(std::cell::Cell::new(IcState::EMPTY));
        idx
    }
    /// Reserve a fresh name-cache slot for a free-name op.
    fn new_name_cache(&mut self) -> u32 {
        self.name_caches.push(std::cell::Cell::new(NameIc::EMPTY));
        (self.name_caches.len() - 1) as u32
    }
    fn emit_store_name(&mut self, name: u32) {
        let cache = self.new_name_cache();
        self.emit(Op::StoreNameCached(name, cache));
    }
    /// Declare every binding a lexical declaration pattern introduces, in source order (slot +
    /// TDZ each, like the plain-identifier path). Only the destructuring subset the compiler
    /// can lower is accepted (see `destructure_store`); anything else bails to the tree-walker.
    fn declare_lexical_pattern(&mut self, pat: &Pattern, is_const: bool) -> CResult {
        match pat {
            Pattern::Ident(name) => {
                if self.homed_pending.remove(name) {
                    // The homed block `let`'s own declaration (see `Compiler::homed_lets`):
                    // in TDZ since entry, no slot, no per-entry Tdz (the block runs at most
                    // once per call by construction). Consumed so any LATER same-name
                    // declaration (a nested for-of head, a sibling block) slot-shadows.
                    return Ok(());
                }
                let slot = self.fresh_slot(name);
                self.scope_bind(name, slot, is_const);
                self.tdz_slots.insert(slot);
                self.emit(Op::Tdz(slot));
                Ok(())
            }
            Pattern::Object(o) => {
                if o.rest.is_some() {
                    return Err(Bail);
                }
                for prop in &o.props {
                    if prop.default.is_some()
                        || !matches!(&prop.key, PropKey::Ident(_) | PropKey::Str(_))
                    {
                        return Err(Bail);
                    }
                    self.declare_lexical_pattern(&prop.value, is_const)?;
                }
                Ok(())
            }
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem {
                            pattern,
                            default: None,
                        } => self.declare_lexical_pattern(pattern, is_const)?,
                        _ => return Err(Bail),
                    }
                }
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    /// Lower a declaration destructuring against the value on the stack (consumed): the
    /// KeyedBindingInitialization subset with plain (non-computed) keys, no defaults, no rest —
    /// per property: Dup + GetProp (the oracle's GetV), recursing into nested object patterns.
    /// The nullish guard throws the oracle's exact TypeError before any read.
    fn destructure_store(&mut self, pat: &Pattern, kind: DeclKind) -> CResult {
        match pat {
            Pattern::Ident(name) => {
                let home = self.home(name).ok_or(Bail)?;
                match home {
                    Home::Slot(slot, _) => {
                        self.emit(Op::StoreLocal(slot));
                    }
                    Home::Env(_) => {
                        let n = self.name_idx(name);
                        if matches!(kind, DeclKind::Var) {
                            self.emit(Op::StoreCap(n));
                        } else {
                            self.emit(Op::StoreCapInit(n));
                        }
                    }
                }
                Ok(())
            }
            Pattern::Object(o) => {
                if o.rest.is_some() {
                    return Err(Bail);
                }
                self.emit(Op::DestructureGuard);
                for prop in &o.props {
                    if prop.default.is_some() {
                        return Err(Bail);
                    }
                    let key: String = match &prop.key {
                        PropKey::Ident(k) => k.clone(),
                        PropKey::Str(k) => k.to_string(),
                        _ => return Err(Bail),
                    };
                    let ki = self.name_idx(&key);
                    self.emit(Op::Dup);
                    let c = self.new_cache();
                    self.emit(Op::GetProp(ki, c));
                    self.destructure_store(&prop.value, kind)?;
                }
                self.emit(Op::Pop);
                Ok(())
            }
            Pattern::Array(elems) => {
                // Batched iterator walk (Op::DestructureArr), then stores in reverse. Batching
                // is only order-unobservable when every leaf is an UNCAPTURED slot (an env-homed
                // leaf's initialization is visible to a later iterator step's next() per spec)
                // and elements are flat idents/holes (a nested pattern's own reads would
                // interleave with the steps), with no defaults (their evaluation interleaves).
                for e in elems.iter() {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem {
                            pattern: Pattern::Ident(n),
                            default: None,
                        } if matches!(self.home(n), Some(Home::Slot(..))) => {}
                        _ => return Err(Bail),
                    }
                }
                self.emit(Op::DestructureArr(elems.len() as u16));
                for e in elems.iter().rev() {
                    match e {
                        ArrayPatElem::Hole => {
                            self.emit(Op::Pop);
                        }
                        ArrayPatElem::Elem { pattern, .. } => {
                            self.destructure_store(pattern, kind)?;
                        }
                        ArrayPatElem::Rest(_) => unreachable!("filtered above"),
                    }
                }
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    /// Emit a call's arguments left to right. `Ok(true)`: the last argument was a spread —
    /// the caller must emit a `CallSpread` op (evaluate-then-expand only matches the spec's
    /// interleaved order when nothing evaluates after the spread part, so any other spread
    /// position bails).
    fn call_args(&mut self, args: &[ArrayElem]) -> Result<bool, Bail> {
        let spread_at = args.iter().position(|a| !matches!(a, ArrayElem::Item(_)));
        if let Some(k) = spread_at {
            if k != args.len() - 1 || !matches!(args[k], ArrayElem::Spread(_)) {
                log_bail("expr", "spread argument (non-final)");
                return Err(Bail);
            }
        }
        for a in args {
            match a {
                ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                ArrayElem::Hole => return Err(Bail),
            }
        }
        Ok(spread_at.is_some())
    }

    /// `delete obj.p` / `delete obj[k]` on plain (non-optional, non-super, public) references;
    /// a non-reference operand evaluates for its effects and deletes to `true`. Identifier
    /// deletes (env bindings) and optional chains stay in the oracle.
    fn delete_expr(&mut self, arg: &Expr) -> CResult {
        match arg {
            Expr::Paren(inner) => self.delete_expr(inner),
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let n = self.name_idx(prop);
                self.emit(Op::DeleteProp(n, self.strict));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::DeleteElem(self.strict));
                Ok(())
            }
            Expr::Ident(_) | Expr::OptionalChain(_) => Err(Bail),
            other => {
                self.expr(other)?;
                self.emit(Op::Pop);
                let k = self.const_idx(Value::Bool(true));
                self.emit(Op::Const(k));
                Ok(())
            }
        }
    }

    /// Compile an optional chain (`a?.b.c`, `r?.m(args)`): each optional link peeks its base —
    /// nullish pops what the link would have consumed and jumps to a shared pad that pushes the
    /// chain's `undefined` result (skipping every later link, key expression, and argument, per
    /// spec). Non-optional links compile as usual. Supported spine: Member/Index reads and
    /// Member-callee method calls; anything else (optional `delete`, `?.()` on a plain callee,
    /// private names, `super`) bails to the tree-walker.
    fn opt_chain(&mut self, e: &Expr, shorts: &mut Vec<usize>) -> CResult {
        match e {
            Expr::Member {
                obj,
                prop,
                optional,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::GetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional,
            } if !matches!(**obj, Expr::Super) => {
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                self.expr(index)?;
                self.emit(Op::GetElem);
                Ok(())
            }
            Expr::Call {
                callee,
                args,
                optional: call_opt,
            } => {
                let Expr::Member {
                    obj,
                    prop,
                    optional,
                } = &**callee
                else {
                    return Err(Bail);
                };
                if matches!(**obj, Expr::Super) || prop.starts_with('#') {
                    return Err(Bail);
                }
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::GetMethod(i, c));
                if *call_opt {
                    // `a.b?.(args)`: the method value is peeked; nullish drops [obj, method].
                    self.opt_link(2, shorts);
                }
                for a in args {
                    let ArrayElem::Item(a) = a else {
                        return Err(Bail);
                    };
                    self.expr(a)?;
                }
                self.emit(Op::CallWithThis(args.len() as u16));
                Ok(())
            }
            // The chain's base (before any `?.` link): an ordinary expression.
            other => self.expr(other),
        }
    }

    /// One optional link: fall through when the top of stack isn't nullish; otherwise pop the
    /// `depth` values the rest of the link would consume and jump to the chain's undefined pad.
    fn opt_link(&mut self, depth: u16, shorts: &mut Vec<usize>) {
        let cont = self.emit(Op::JumpIfNotNullishPeek(0));
        for _ in 0..depth {
            self.emit(Op::Pop);
        }
        shorts.push(self.emit(Op::Jump(0)));
        self.patch(cont);
    }

    /// The local slot for a fused element access (`x[k]` → `GetElemLocal`), or `None` to use
    /// the generic ops. Fusing defers the base-local read past the key/value evaluation, so it
    /// requires: the base is an Ident homed in a slot that can never be in TDZ (a param or a
    /// `var` — no early throw to reorder; see `tdz_slots`), and no `deps` expression can
    /// reassign that local (calls can't — slot locals are unobservable outside the function;
    /// only an explicit assignment/update in the key/value expressions themselves could, and
    /// `no_assign_to` rejects those).
    fn fused_elem_slot(&self, obj: &Expr, deps: &[&Expr]) -> Option<u16> {
        let Expr::Ident(name) = obj else { return None };
        let Some(Home::Slot(slot, _)) = self.home(name) else {
            return None;
        };
        if self.tdz_slots.contains(&slot) {
            return None;
        }
        if deps.iter().all(|d| no_assign_to(d, name)) {
            Some(slot)
        } else {
            None
        }
    }
    fn fresh_slot(&mut self, name: &str) -> u16 {
        let slot = self.slot_names.len() as u16;
        self.slot_names.push(Rc::from(name));
        slot
    }
    fn scope_bind(&mut self, name: &str, slot: u16, is_const: bool) {
        if self.scopes.is_empty() {
            self.scopes.push(Vec::new());
        }
        let top = self.scopes.last_mut().unwrap();
        if let Some(e) = top.iter_mut().find(|(n, ..)| n == name) {
            *e = (name.to_string(), slot, is_const);
        } else {
            top.push((name.to_string(), slot, is_const));
        }
    }
    fn lookup(&self, name: &str) -> Option<(u16, bool)> {
        for scope in self.scopes.iter().rev() {
            if let Some((_, slot, k)) = scope.iter().rev().find(|(n, ..)| n == name) {
                return Some((*slot, *k));
            }
        }
        None
    }
    fn env_bind(&mut self, name: &str, is_const: bool) {
        self.env_names.insert(name.to_string(), is_const);
    }
    fn env_has(&self, name: &str) -> bool {
        self.env_names.contains_key(name)
    }
    /// Resolve a local: innermost slot scope first (block lexicals shadow captured names — a
    /// captured block lexical bails compile, so every env name is function-scope-wide).
    fn home(&self, name: &str) -> Option<Home> {
        if let Some((slot, k)) = self.lookup(name) {
            return Some(Home::Slot(slot, k));
        }
        self.env_names.get(name).map(|k| Home::Env(*k))
    }
    fn const_idx(&mut self, v: Value) -> u32 {
        self.consts.push(v);
        (self.consts.len() - 1) as u32
    }
    fn name_idx(&mut self, name: &str) -> u32 {
        if let Some(i) = self.names.iter().position(|n| &**n == name) {
            return i as u32;
        }
        self.names.push(intern_name(name));
        (self.names.len() - 1) as u32
    }
    /// Compile `test` and a jump taken when it is falsy; returns the jump to patch. A comparison
    /// fuses into the jump ([`Op::JumpIfNotCmp`]): nothing inside `test` can jump between the
    /// compare and the branch, since a comparison's own operands end at the compare.
    fn jump_if_false(&mut self, test: &Expr) -> Result<usize, Bail> {
        if let Expr::Binary { op, left, right } = test {
            if let Some(kind) = CmpKind::of(op) {
                if typeof_test(op, left, right).is_none() {
                    let at = self.ops.len();
                    self.expr(left)?;
                    self.expr(right)?;
                    // Both operands single loads: nothing can target the second, so the three ops
                    // collapse into one at the first's index (where a loop head may point).
                    if self.ops.len() == at + 2 {
                        let fused = match (self.ops[at], self.ops[at + 1]) {
                            (Op::LoadLocal(a), Op::LoadLocal(b)) => {
                                Some(Op::JumpIfNotCmpLL(kind, a, b, 0))
                            }
                            (Op::LoadLocal(a), Op::Const(k)) => {
                                Some(Op::JumpIfNotCmpLK(kind, a, k, 0))
                            }
                            _ => None,
                        };
                        if let Some(op) = fused {
                            self.ops.truncate(at);
                            return Ok(self.emit(op));
                        }
                    }
                    return Ok(self.emit(Op::JumpIfNotCmp(kind, 0)));
                }
            }
        }
        self.expr(test)?;
        Ok(self.emit(Op::JumpIfFalse(0)))
    }

    fn patch(&mut self, at: usize) {
        let target = self.ops.len() as u32;
        match &mut self.ops[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfNotCmp(_, t)
            | Op::JumpIfNotCmpLL(.., t)
            | Op::JumpIfNotCmpLK(.., t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t) => *t = target,
            _ => unreachable!("patching a non-jump"),
        }
    }

    /// Declare the function body's top-level `let`/`const`: captured ones home in the activation
    /// env (inserted in TDZ by `make_run_env`), the rest get TDZ slots. Function declarations
    /// were already handled by the hoist plan; classes and `using` bail.
    fn declare_body_lexicals(
        &mut self,
        stmts: &[Stmt],
        captured: &std::collections::HashSet<String>,
    ) -> CResult {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: kind @ (DeclKind::Let | DeclKind::Const),
                    decls,
                } => {
                    let is_const = matches!(kind, DeclKind::Const);
                    for (pat, _) in decls {
                        // Patterns declare every bound ident; a captured one homes in the
                        // activation env like the plain-ident path.
                        let mut names = std::collections::HashSet::new();
                        pat_idents(pat, &mut names);
                        if !matches!(pat, Pattern::Ident(_)) {
                            // The execution lowering only handles the object-pattern subset.
                            if names.iter().any(|n| captured.contains(n)) {
                                log_bail("body-lexicals", "captured destructured lexical");
                                return Err(Bail);
                            }
                            self.declare_lexical_pattern(pat, is_const)?;
                            continue;
                        }
                        let Pattern::Ident(name) = pat else {
                            unreachable!()
                        };
                        if captured.contains(name) {
                            self.cap_inits
                                .push(CapInit::Lexical(Rc::from(name.as_str()), is_const));
                            self.env_bind(name, is_const);
                        } else {
                            let slot = self.fresh_slot(name);
                            self.scope_bind(name, slot, is_const);
                            self.tdz_slots.insert(slot);
                            self.emit(Op::Tdz(slot));
                        }
                    }
                }
                Stmt::VarDecl {
                    kind: DeclKind::Using | DeclKind::AwaitUsing,
                    ..
                }
                | Stmt::ClassDecl(_) => return Err(Bail),
                Stmt::FuncDecl(_) => {} // hoisted — created at entry
                _ => {}
            }
        }
        Ok(())
    }

    /// Declare a statement list's `let`/`const` as TDZ slots (block entry).
    fn declare_block_lexicals(&mut self, stmts: &[Stmt]) -> CResult {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const,
                    decls,
                } => {
                    let is_const = matches!(
                        s,
                        Stmt::VarDecl {
                            kind: DeclKind::Const,
                            ..
                        }
                    );
                    for (pat, _) in decls {
                        self.declare_lexical_pattern(pat, is_const)?;
                    }
                }
                Stmt::VarDecl {
                    kind: DeclKind::Using | DeclKind::AwaitUsing,
                    ..
                }
                | Stmt::ClassDecl(_)
                | Stmt::FuncDecl(_) => return Err(Bail),
                _ => {}
            }
        }
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> CResult {
        match s {
            Stmt::Expr(e) => self.expr_stmt(e),
            Stmt::Empty | Stmt::Debugger => Ok(()),
            // Top-level function declarations were hoisted (created at entry); block-level ones
            // never reach here (declare_block_lexicals bails first).
            Stmt::FuncDecl(_) => Ok(()),
            Stmt::VarDecl { kind, decls } => {
                if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                    return Err(Bail);
                }
                for (pat, init) in decls {
                    let Pattern::Ident(name) = pat else {
                        // Destructuring declaration: evaluate the initializer, then lower the
                        // pattern against it (a pattern without an initializer is a parse error).
                        let Some(e) = init else { return Err(Bail) };
                        self.expr(e)?;
                        self.destructure_store(pat, *kind)?;
                        continue;
                    };
                    let home = self.home(name).ok_or(Bail)?;
                    match init {
                        Some(e) => self.named_expr(e, name)?,
                        // `var x;` leaves an existing binding alone; `let x;` initializes.
                        None => {
                            if matches!(kind, DeclKind::Var) {
                                continue;
                            }
                            self.emit(Op::Undef);
                        }
                    }
                    match home {
                        Home::Slot(slot, _) => {
                            self.emit(Op::StoreLocal(slot));
                        }
                        Home::Env(_) => {
                            let n = self.name_idx(name);
                            // A lexical declaration initializes (clearing TDZ); a `var` writes an
                            // already-initialized binding.
                            if matches!(kind, DeclKind::Var) {
                                self.emit(Op::StoreCap(n));
                            } else {
                                self.emit(Op::StoreCapInit(n));
                            }
                        }
                    }
                }
                Ok(())
            }
            Stmt::Return(arg) => {
                // A return crossing compiled for-of loops must IteratorClose them (the value
                // evaluates first, spec order). One level is modeled exactly; more would need
                // the spec's cascading throw-mode closes — bail to the tree-walker.
                let fors: Vec<(u16, u32)> = self
                    .loops
                    .iter()
                    .filter_map(|c| c.foreach_iter.map(|it| (it, c.body_try_depth)))
                    .collect();
                if fors.len() > 1 {
                    return Err(Bail);
                }
                match arg {
                    Some(e) => self.expr(e)?,
                    None => {
                        self.emit(Op::Undef);
                    }
                }
                if let Some(&(iter_s, body_depth)) = fors.first() {
                    // Pop the regions inside the loop body, its handler, then close — a close
                    // error propagates to handlers *outside* the loop, replacing the return.
                    for _ in body_depth..self.try_depth {
                        self.emit(Op::PopHandler);
                    }
                    self.emit(Op::PopHandler);
                    self.emit(Op::IterCloseL(iter_s));
                }
                self.emit(Op::Return);
                Ok(())
            }
            Stmt::Throw(e) => {
                self.expr(e)?;
                self.emit(Op::Throw);
                Ok(())
            }
            Stmt::If { test, cons, alt } => {
                let jf = self.jump_if_false(test)?;
                self.stmt(cons)?;
                match alt {
                    Some(a) => {
                        let jend = self.emit(Op::Jump(0));
                        self.patch(jf);
                        self.stmt(a)?;
                        self.patch(jend);
                    }
                    None => self.patch(jf),
                }
                Ok(())
            }
            Stmt::Block(body) => {
                self.scopes.push(Vec::new());
                let r = self.block_body(body);
                self.scopes.pop();
                r
            }
            Stmt::While { test, body } => {
                let labels = std::mem::take(&mut self.pending_labels);
                let start = self.ops.len();
                let jf = self.jump_if_false(test)?;
                self.loops.push(LoopCtx {
                    labels,
                    ..LoopCtx::default()
                });
                let r = self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                r?;
                for c in ctx.continues {
                    match &mut self.ops[c] {
                        Op::Jump(t) => *t = start as u32,
                        _ => unreachable!(),
                    }
                }
                self.emit(Op::Jump(start as u32));
                self.patch(jf);
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            Stmt::DoWhile { body, test } => {
                let labels = std::mem::take(&mut self.pending_labels);
                let start = self.ops.len();
                self.loops.push(LoopCtx {
                    labels,
                    ..LoopCtx::default()
                });
                let r = self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                r?;
                let cont = self.ops.len();
                for c in ctx.continues {
                    match &mut self.ops[c] {
                        Op::Jump(t) => *t = cont as u32,
                        _ => unreachable!(),
                    }
                }
                let jf = self.jump_if_false(test)?;
                self.emit(Op::Jump(start as u32));
                self.patch(jf);
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                self.scopes.push(Vec::new());
                let r = self.for_loop(init.as_deref(), test.as_ref(), update.as_ref(), body);
                self.scopes.pop();
                r
            }
            Stmt::Break(None) => {
                let idx = self.loops.len().checked_sub(1).ok_or(Bail)?;
                self.emit_exit_cleanup(idx, false)?;
                let j = self.emit(Op::Jump(0));
                self.loops[idx].breaks.push(j);
                Ok(())
            }
            Stmt::Continue(None) => {
                // `continue` skips switch contexts: it targets the innermost enclosing *loop*.
                let idx = self.loops.iter().rposition(|c| !c.is_switch).ok_or(Bail)?;
                self.emit_exit_cleanup(idx, true)?;
                let j = self.emit(Op::Jump(0));
                self.loops[idx].continues.push(j);
                Ok(())
            }
            // Labelled break/continue: jump to the loop on the stack that carries the target label.
            // A `break` to a labelled *block* (not a loop) isn't modeled here — no ctx matches, so
            // it bails to the interpreter.
            Stmt::Break(Some(name)) => {
                let idx = self
                    .loops
                    .iter()
                    .rposition(|c| c.labels.iter().any(|l| l == name))
                    .ok_or(Bail)?;
                self.emit_exit_cleanup(idx, false)?;
                let j = self.emit(Op::Jump(0));
                self.loops[idx].breaks.push(j);
                Ok(())
            }
            Stmt::Continue(Some(name)) => {
                // A labelled continue must target a loop — a label on a switch is only a break
                // target (the parser rejects `continue` to it; not-found bails to the oracle).
                let idx = self
                    .loops
                    .iter()
                    .rposition(|c| !c.is_switch && c.labels.iter().any(|l| l == name))
                    .ok_or(Bail)?;
                self.emit_exit_cleanup(idx, true)?;
                let j = self.emit(Op::Jump(0));
                self.loops[idx].continues.push(j);
                Ok(())
            }
            // A label naming a loop or switch attaches to that context; stacked labels
            // (`a: b: for`) accumulate through the recursion. A label on any other statement bails.
            Stmt::Labeled { label, body } => match &**body {
                Stmt::While { .. }
                | Stmt::DoWhile { .. }
                | Stmt::For { .. }
                | Stmt::Switch { .. }
                | Stmt::Labeled { .. } => {
                    self.pending_labels.push(label.clone());
                    self.stmt(body)
                }
                _ => Err(Bail),
            },
            // `switch`: the discriminant lands in a hidden slot; case tests run in source order
            // (exactly the oracle's two-phase evaluation), then bodies are laid out contiguously
            // so fall-through is just falling through. Any lexical/class/function declaration
            // directly in a case body bails — the oracle gives all cases one shared block scope
            // whose TDZ interleavings slots don't model.
            Stmt::Switch { disc, cases } => self.switch_statement(disc, cases),
            // `try { ... } catch (e?) { ... }` — no `finally` (bails), catch param an ident or none.
            // On a throw in the try region the VM unwinds to `catch_pc` with the exception pushed.
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                if finalizer.is_some() {
                    log_bail("stmt", "try/finally");
                    return Err(Bail);
                }
                let Some((param, catch_body)) = handler else {
                    return Err(Bail); // `try`/`finally` with no `catch`
                };
                if matches!(param, Some(p) if !matches!(p, Pattern::Ident(_))) {
                    return Err(Bail); // destructuring catch param
                }
                let push = self.emit(Op::PushHandler(0));
                self.try_depth += 1;
                self.scopes.push(Vec::new());
                let tr = self.block_body(block);
                self.scopes.pop();
                tr?;
                self.emit(Op::PopHandler);
                self.try_depth -= 1;
                let jmp_after = self.emit(Op::Jump(0));
                // Catch entry: the exception is on the stack.
                let catch_pc = self.ops.len() as u32;
                match &mut self.ops[push] {
                    Op::PushHandler(t) => *t = catch_pc,
                    _ => unreachable!(),
                }
                self.scopes.push(Vec::new());
                match param {
                    Some(Pattern::Ident(name)) => {
                        let slot = self.fresh_slot(name);
                        self.scope_bind(name, slot, false);
                        self.emit(Op::StoreLocal(slot));
                    }
                    _ => {
                        self.emit(Op::Pop); // no binding (or `catch {}`): discard the exception
                    }
                }
                let cr = self.block_body(catch_body);
                self.scopes.pop();
                cr?;
                self.patch(jmp_after);
                Ok(())
            }
            Stmt::ForInOf {
                decl: Some(kind @ (DeclKind::Let | DeclKind::Const)),
                left: Pattern::Ident(name),
                right,
                of: false,
                is_await: false,
                body,
            } => self.for_in_statement(*kind, name, right, body),
            // `for (x of it)`: the iterator and its `next` live in hidden slots; each step is
            // IterStepL + JumpIfFalse (existing branch machinery in both tiers); the body runs
            // under a per-iteration handler whose pad closes the iterator in throw mode and
            // rethrows. Exhaustion closes nothing (spec); break/return close via
            // `emit_exit_cleanup` / the Return arm. Other for-in heads, `for await`, and
            // captured loop variables stay on the tree-walker.
            Stmt::ForInOf {
                decl,
                left,
                right,
                of: true,
                is_await: false,
                body,
            } => {
                let labels = std::mem::take(&mut self.pending_labels);
                // The loop variable: a declaration binds a fresh (uncaptured — else the
                // per-iteration env freshness matters and we bail) slot scoped to the loop; a
                // bare identifier assigns an existing binding or a free name. Spec order for a
                // lexical declaration: the fresh binding exists — in TDZ — while the iterable
                // expression evaluates (`for (const x of [x])` throws a ReferenceError), so the
                // scope and Tdz emit BEFORE `right`.
                self.scopes.push(Vec::new());
                enum Bind {
                    Slot(u16),
                    Cap(u32),
                    Name(u32),
                    /// Destructuring lexical head: bound by `destructure_store` INSIDE the
                    /// body's handler region (a binding throw must IteratorClose in throw mode,
                    /// which is exactly what the body's abort pad does).
                    Pattern(DeclKind),
                }
                let bind = match (left, decl) {
                    (_, Some(DeclKind::Using | DeclKind::AwaitUsing)) => {
                        self.scopes.pop();
                        return Err(Bail);
                    }
                    (Pattern::Ident(name), Some(kind)) => {
                        // An env-homed name blocks a head slot — except a homed block `let`
                        // (`Compiler::homed_lets`), which a fresh slot shadows correctly.
                        if self.env_names.contains_key(name) && !self.homed_lets.contains(name) {
                            self.scopes.pop();
                            return Err(Bail);
                        }
                        let slot = self.fresh_slot(name);
                        self.scope_bind(name, slot, matches!(kind, DeclKind::Const));
                        if matches!(kind, DeclKind::Let | DeclKind::Const) {
                            self.tdz_slots.insert(slot);
                            self.emit(Op::Tdz(slot));
                        }
                        Bind::Slot(slot)
                    }
                    (pat, Some(kind @ (DeclKind::Let | DeclKind::Const))) => {
                        // A destructuring lexical head: fresh uncaptured slots for every leaf,
                        // declared (in TDZ) before `right` like the ident path. `var` patterns
                        // would have to write hoisted function-scope bindings — those stay in
                        // the oracle.
                        let mut leaf_names = std::collections::HashSet::new();
                        pat_idents(pat, &mut leaf_names);
                        if leaf_names
                            .iter()
                            .any(|n| self.env_names.contains_key(n) && !self.homed_lets.contains(n))
                            || self
                                .declare_lexical_pattern(pat, matches!(kind, DeclKind::Const))
                                .is_err()
                        {
                            self.scopes.pop();
                            return Err(Bail);
                        }
                        Bind::Pattern(*kind)
                    }
                    (_, Some(DeclKind::Var)) => {
                        self.scopes.pop();
                        return Err(Bail);
                    }
                    (Pattern::Ident(name), None) => match self.home(name) {
                        Some(Home::Slot(slot, is_const)) => {
                            if is_const {
                                self.scopes.pop();
                                return Err(Bail);
                            }
                            Bind::Slot(slot)
                        }
                        Some(Home::Env(is_const)) => {
                            if is_const {
                                self.scopes.pop();
                                return Err(Bail);
                            }
                            Bind::Cap(self.name_idx(name))
                        }
                        None => Bind::Name(self.name_idx(name)),
                    },
                    // Destructuring *assignment* head (`for ([a, b] of xs)`) — oracle.
                    (_, None) => {
                        self.scopes.pop();
                        return Err(Bail);
                    }
                };
                let er = self.expr(right);
                if er.is_err() {
                    self.scopes.pop();
                    return er;
                }
                let iter_s = self.fresh_slot("%iter%");
                let next_s = self.fresh_slot("%next%");
                self.emit(Op::GetIter);
                self.emit(Op::StoreLocal(next_s));
                self.emit(Op::StoreLocal(iter_s));
                self.loops.push(LoopCtx {
                    labels,
                    entry_try_depth: self.try_depth,
                    foreach_iter: Some(iter_s),
                    ..Default::default()
                });
                let loop_head = self.ops.len();
                self.emit(Op::IterStepL(iter_s, next_s));
                let jexit = self.emit(Op::JumpIfFalse(0));
                match bind {
                    Bind::Slot(slot) => {
                        self.emit(Op::StoreLocal(slot));
                    }
                    Bind::Cap(n) => {
                        self.emit(Op::StoreCap(n));
                    }
                    Bind::Name(n) => {
                        self.emit_store_name(n);
                    }
                    Bind::Pattern(_) => {} // bound below, inside the handler region
                }
                let push = self.emit(Op::PushHandler(0));
                self.try_depth += 1;
                self.loops.last_mut().expect("just pushed").body_try_depth = self.try_depth;
                let r = match bind {
                    Bind::Pattern(kind) => self
                        .destructure_store(left, kind)
                        .and_then(|()| self.stmt(body)),
                    _ => self.stmt(body),
                };
                let ctx = self.loops.pop().expect("just pushed");
                self.scopes.pop();
                r?;
                self.emit(Op::PopHandler);
                self.try_depth -= 1;
                // continues re-enter at the step (the loop head re-pushes the body handler —
                // their cleanup already popped it).
                for j in ctx.continues {
                    match &mut self.ops[j] {
                        Op::Jump(t) => *t = loop_head as u32,
                        _ => unreachable!("continue is a jump"),
                    }
                }
                self.emit(Op::Jump(0));
                let jback = self.ops.len() - 1;
                match &mut self.ops[jback] {
                    Op::Jump(t) => *t = loop_head as u32,
                    _ => unreachable!(),
                }
                // The body's catch pad: swallow-close + rethrow (always abrupt).
                let abort_pc = self.ops.len() as u32;
                match &mut self.ops[push] {
                    Op::PushHandler(t) => *t = abort_pc,
                    _ => unreachable!(),
                }
                self.emit(Op::IterAbortL(iter_s));
                // Exhaustion lands here (the step's bool was false): drop the undefined
                // placeholder the step pushed; no close on a completed iterator.
                self.patch(jexit);
                self.emit(Op::Pop);
                // Breaks jump here too — their cleanup (pop handler + close) ran at the site.
                let after = self.ops.len() as u32;
                for j in ctx.breaks {
                    match &mut self.ops[j] {
                        Op::Jump(t) => *t = after,
                        _ => unreachable!("break is a jump"),
                    }
                }
                Ok(())
            }
            other => {
                log_bail_node("stmt", other, 60);
                Err(Bail)
            }
        }
    }

    /// Emit the bookkeeping a `break`/`continue` targeting `self.loops[target]` must run before
    /// its jump: pop every `try`/for-of-body handler region opened since the target's entry (a
    /// stale handler would catch unrelated throws later in the frame), and IteratorClose each
    /// for-of iterator being abandoned — the target's own iterator too for a `break`, but not
    /// for a `continue` (the loop keeps iterating). Crossing more than one for-of level bails:
    /// the spec cascades close errors through the *outer* loop's throw-mode close, which this
    /// flat emission doesn't model (the tree-walker gets it right by unwinding).
    fn emit_exit_cleanup(&mut self, target: usize, is_continue: bool) -> CResult {
        // For-of levels whose iterator is abandoned by this jump.
        let closes: Vec<(usize, u16, u32)> = self
            .loops
            .iter()
            .enumerate()
            .skip(if is_continue { target + 1 } else { target })
            .filter_map(|(k, c)| c.foreach_iter.map(|it| (k, it, c.body_try_depth)))
            .collect();
        if closes.len() > 1 {
            return Err(Bail);
        }
        let mut depth_now = self.try_depth;
        if let Some(&(_, iter_s, body_depth)) = closes.last() {
            // Pop the regions inside the for-of body, then its own body handler, then close.
            for _ in body_depth..depth_now {
                self.emit(Op::PopHandler);
            }
            self.emit(Op::PopHandler);
            self.emit(Op::IterCloseL(iter_s));
            depth_now = body_depth - 1;
        }
        // Remaining regions down to the target's entry (plain `try`s between the loops — and for
        // a continue to a for-of, its own body handler, which the loop head re-pushes).
        let floor = self.loops[target].entry_try_depth;
        for _ in floor..depth_now {
            self.emit(Op::PopHandler);
        }
        Ok(())
    }

    fn block_body(&mut self, body: &[Stmt]) -> CResult {
        self.declare_block_lexicals(body)?;
        for s in body {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn for_loop(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> CResult {
        // Claim any labels from an enclosing `Stmt::Labeled` before the head runs, so they land on
        // this loop's context (the head itself introduces no labelled break/continue targets).
        let labels = std::mem::take(&mut self.pending_labels);
        match init {
            Some(ForInit::VarDecl { kind, decls }) => {
                if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                    return Err(Bail);
                }
                if matches!(kind, DeclKind::Let | DeclKind::Const) {
                    for (pat, _) in decls {
                        let Pattern::Ident(name) = pat else {
                            return Err(Bail);
                        };
                        let slot = self.fresh_slot(name);
                        self.scope_bind(name, slot, matches!(kind, DeclKind::Const));
                        self.tdz_slots.insert(slot);
                        self.emit(Op::Tdz(slot));
                    }
                }
                for (pat, initv) in decls {
                    let Pattern::Ident(name) = pat else {
                        return Err(Bail);
                    };
                    let (slot, _) = self.lookup(name).ok_or(Bail)?;
                    match initv {
                        Some(e) => {
                            self.expr(e)?;
                            self.emit(Op::StoreLocal(slot));
                        }
                        None => {
                            if !matches!(kind, DeclKind::Var) {
                                self.emit(Op::Undef);
                                self.emit(Op::StoreLocal(slot));
                            }
                        }
                    }
                }
            }
            Some(ForInit::Expr(e)) => {
                self.expr_stmt(e)?;
            }
            None => {}
        }
        let start = self.ops.len();
        let jf = match test {
            Some(t) => Some(self.jump_if_false(t)?),
            None => None,
        };
        self.loops.push(LoopCtx {
            labels,
            ..LoopCtx::default()
        });
        let r = self.stmt(body);
        let ctx = self.loops.pop().unwrap();
        r?;
        let cont = self.ops.len();
        for c in ctx.continues {
            match &mut self.ops[c] {
                Op::Jump(t) => *t = cont as u32,
                _ => unreachable!(),
            }
        }
        if let Some(u) = update {
            self.expr_stmt(u)?;
        }
        self.emit(Op::Jump(start as u32));
        if let Some(jf) = jf {
            self.patch(jf);
        }
        for b in ctx.breaks {
            self.patch(b);
        }
        Ok(())
    }

    /// Compile an expression whose value is discarded (an expression statement, or a `for`
    /// header's init / update). Assignments and `++`/`--` to a local drop their producing `Dup`
    /// (and the trailing `Pop`); everything else falls back to `expr` + `Pop`. Semantically
    /// identical to `self.expr(e)?; self.emit(Op::Pop)` — the only difference is the unobservable
    /// result value.
    fn expr_stmt(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Paren(inner) => return self.expr_stmt(inner),
            // A comma expression as a statement: every operand is evaluated for effect only.
            Expr::Seq(exprs) => {
                for ex in exprs {
                    self.expr_stmt(ex)?;
                }
                return Ok(());
            }
            Expr::Update { op, arg, .. } => {
                let kind = match *op {
                    "++" => UpdKind::IncDiscard,
                    "--" => UpdKind::DecDiscard,
                    _ => return Err(Bail),
                };
                return self.update_target(arg, kind);
            }
            Expr::Assign { op, target, value } => {
                return self.assign_discard(op, target, value);
            }
            _ => {}
        }
        self.expr(e)?;
        self.emit(Op::Pop);
        Ok(())
    }

    /// Compile a discarded assignment: the fast `Dup`-free lowering when the target is a plain
    /// local / free name / `obj.x` / `obj[k]`, otherwise the generic value-producing `assign`
    /// followed by `Pop` (identical to `self.expr(assign)?; Pop`).
    fn assign_discard(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if self.try_assign_discard(op, target, value)? {
            return Ok(());
        }
        self.assign(op, target, value)?;
        self.emit(Op::Pop);
        Ok(())
    }

    /// Fast lowering for a discarded assignment (no `Dup`, no trailing `Pop`). Returns `Ok(true)`
    /// when it emitted the assignment, `Ok(false)` to defer to the generic `assign` + `Pop` path
    /// (which handles — or itself bails on — the forms not covered here). Any `Bail` from a
    /// compiled sub-expression propagates: the generic path would bail identically.
    fn try_assign_discard(&mut self, op: &str, target: &Expr, value: &Expr) -> Result<bool, Bail> {
        // Logical-assignment short-circuits; leave it to the generic path (which bails).
        if matches!(op, "&&=" | "||=" | "??=") {
            return Ok(false);
        }
        match target {
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, is_const)) => {
                    if is_const {
                        return Ok(false);
                    }
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreLocal(slot));
                    Ok(true)
                }
                Some(Home::Env(is_const)) => {
                    if is_const {
                        return Ok(false); // runtime TypeError — the oracle's business
                    }
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadCap(n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreCap(n));
                    Ok(true)
                }
                None => {
                    let i = self.name_idx(name);
                    if op == "=" {
                        // StoreName already consumes the value without re-pushing it.
                        self.named_expr(value, name)?;
                    } else {
                        // Resolve/read before the RHS, as compound assignment requires. Compiled
                        // closures under `with` are rejected at entry and direct eval in this
                        // body prevents compilation, so no nearer binding can appear between
                        // this read and StoreName; re-resolution names the same Reference.
                        let c = self.new_name_cache();
                        self.emit(Op::LoadName(i, c));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit_store_name(i);
                    Ok(true)
                }
            },
            // Receiver-direct statement stores: `this.x = v` (always safe — `this` can't be
            // reassigned) and `slotlocal.x = v` when the RHS provably can't reassign the local
            // (the receiver is read at set time, after the RHS — evaluation order must agree).
            Expr::Member {
                obj: mobj,
                prop,
                optional: false,
            } if op == "="
                && !prop.starts_with('#')
                && match &**mobj {
                    Expr::This => self.direct_this_allowed(),
                    Expr::Ident(name) => {
                        matches!(self.home(name), Some(Home::Slot(..))) && no_assign_to(value, name)
                    }
                    _ => false,
                } =>
            {
                self.expr(value)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                match &**mobj {
                    Expr::This => {
                        self.uses_this = true;
                        self.emit(Op::SetPropThisDrop(i, c));
                    }
                    Expr::Ident(name) => {
                        let Some(Home::Slot(slot, _)) = self.home(name) else {
                            unreachable!()
                        };
                        self.emit(Op::SetPropLocalDrop(slot, i, c));
                    }
                    _ => unreachable!(),
                }
                Ok(true)
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                if op == "+=" {
                    // Fused append: same evaluation order (read before RHS), and the op itself
                    // falls back to the generic Add + store when anything isn't plain strings.
                    self.emit(Op::Dup);
                    let cg = self.new_cache();
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    let c = self.new_cache();
                    self.emit(Op::AppendProp(i, c));
                    return Ok(true);
                }
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::Dup);
                    let cg = self.new_cache();
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                let c = self.new_cache();
                self.emit(Op::SetPropDrop(i, c));
                Ok(true)
            }

            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref(), value]) {
                    self.expr(index)?;
                    if op == "=" {
                        self.expr(value)?;
                    } else {
                        // Compound: coerce a side-effecting key once (Num keys pass raw), then
                        // read-modify-write against the slot base — one Dup, no receiver churn.
                        self.emit(Op::ToPropKeyLocal(slot));
                        self.emit(Op::Dup);
                        self.emit(Op::GetElemLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::SetElemLocalDrop(slot));
                    return Ok(true);
                }
                self.expr(obj)?;
                self.expr(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::ToPropKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetElemDrop);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Emit a closure over the current environment; `name` applies NamedEvaluation to an
    /// anonymous function expression (`var f = function(){}` → `f.name === "f"`).
    fn emit_closure(&mut self, f: &Rc<Function>, name: Option<&str>) {
        let fidx = self.funcs.len() as u32;
        self.funcs.push(f.clone());
        let name_idx = match name {
            Some(n) if f.name.is_none() && !f.is_method => self.name_idx(n),
            _ => u32::MAX,
        };
        self.emit(Op::MakeClosure(fidx, name_idx));
    }

    /// Compile a value expression in a naming position (declaration/assignment to `name`).
    fn named_expr(&mut self, e: &Expr, name: &str) -> CResult {
        if let Expr::Func(f) = e {
            self.emit_closure(f, Some(name));
            return Ok(());
        }
        self.expr(e)
    }

    fn expr(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Func(f) => {
                self.emit_closure(f, None);
                Ok(())
            }
            Expr::Num(n) => {
                let i = self.const_idx(Value::Num(*n));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Str(s) => {
                let i = self.const_idx(Value::Str(s.clone().into()));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Bool(b) => {
                let i = self.const_idx(Value::Bool(*b));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Null => {
                let i = self.const_idx(Value::Null);
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Undefined => {
                self.emit(Op::Undef);
                Ok(())
            }
            Expr::BigInt(n) => {
                let i = self.const_idx(Value::BigInt(n.clone()));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Ident(name) => {
                match self.home(name) {
                    Some(Home::Slot(slot, _)) => {
                        self.emit(Op::LoadLocal(slot));
                    }
                    Some(Home::Env(_)) => {
                        let i = self.name_idx(name);
                        self.emit(Op::LoadCap(i));
                    }
                    None => {
                        let i = self.name_idx(name);
                        let c = self.new_name_cache();
                        self.emit(Op::LoadName(i, c));
                    }
                };
                Ok(())
            }
            Expr::This => {
                self.emit_this();
                Ok(())
            }
            Expr::Paren(inner) => self.expr(inner),
            Expr::Seq(exprs) => {
                for (k, ex) in exprs.iter().enumerate() {
                    self.expr(ex)?;
                    if k + 1 < exprs.len() {
                        self.emit(Op::Pop);
                    }
                }
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                // Receiver-direct forms: `this.x` and `slotlocal.x` skip the operand-stack
                // round trip (push + refcount bump + drop) entirely.
                match &**obj {
                    Expr::This if self.direct_this_allowed() => {
                        self.uses_this = true;
                        let i = self.name_idx(prop);
                        let c = self.new_cache();
                        self.emit(Op::GetPropThis(i, c));
                        return Ok(());
                    }
                    Expr::Ident(name) => {
                        if let Some(Home::Slot(slot, _)) = self.home(name) {
                            let i = self.name_idx(prop);
                            let c = self.new_cache();
                            self.emit(Op::GetPropLocal(slot, i, c));
                            return Ok(());
                        }
                    }
                    _ => {}
                }
                self.expr(obj)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::GetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref()]) {
                    self.expr(index)?;
                    self.emit(Op::GetElemLocal(slot));
                } else {
                    self.expr(obj)?;
                    self.expr(index)?;
                    self.emit(Op::GetElem);
                }
                Ok(())
            }
            Expr::Binary { op, left, right } => {
                if let Some((arg, kind, negated)) = typeof_test(op, left, right) {
                    // `typeof freeName` must not throw for an unresolvable name; that path keeps
                    // TypeofName and the ordinary string comparison.
                    let free = matches!(arg, Expr::Ident(n) if self.home(n).is_none());
                    if !free {
                        self.expr(arg)?;
                        self.emit(Op::TypeofIs(kind, negated));
                        return Ok(());
                    }
                }
                self.expr(left)?;
                self.expr(right)?;
                let bop = match *op {
                    "+" => Op::Add,
                    "-" => Op::Sub,
                    "*" => Op::Mul,
                    "/" => Op::Div,
                    "%" => Op::Mod,
                    "&" => Op::BitAnd,
                    "|" => Op::BitOr,
                    "^" => Op::BitXor,
                    "<<" => Op::Shl,
                    ">>" => Op::Shr,
                    ">>>" => Op::UShr,
                    "<" => Op::Lt,
                    ">" => Op::Gt,
                    "<=" => Op::Le,
                    ">=" => Op::Ge,
                    "==" => Op::EqEq,
                    "!=" => Op::NotEq,
                    "===" => Op::StrictEq,
                    "!==" => Op::StrictNotEq,
                    "instanceof" => Op::InstanceOf(self.new_single_cache()),
                    other => {
                        let i = self.name_idx(other);
                        Op::GenBin(i)
                    }
                };
                self.emit(bop);
                Ok(())
            }
            Expr::Logical { op, left, right } => {
                self.expr(left)?;
                let j = match *op {
                    "&&" => self.emit(Op::JumpIfFalsePeek(0)),
                    "||" => self.emit(Op::JumpIfTruePeek(0)),
                    "??" => self.emit(Op::JumpIfNotNullishPeek(0)),
                    _ => return Err(Bail),
                };
                self.emit(Op::Pop);
                self.expr(right)?;
                self.patch(j);
                Ok(())
            }
            Expr::Cond { test, cons, alt } => {
                let jf = self.jump_if_false(test)?;
                self.expr(cons)?;
                let jend = self.emit(Op::Jump(0));
                self.patch(jf);
                self.expr(alt)?;
                self.patch(jend);
                Ok(())
            }
            Expr::Unary { op, arg } => {
                match *op {
                    "-" => {
                        self.expr(arg)?;
                        self.emit(Op::Neg);
                    }
                    "+" => {
                        self.expr(arg)?;
                        self.emit(Op::Plus);
                    }
                    "!" => {
                        self.expr(arg)?;
                        self.emit(Op::Not);
                    }
                    "~" => {
                        self.expr(arg)?;
                        self.emit(Op::BitNot);
                    }
                    "void" => {
                        self.expr(arg)?;
                        self.emit(Op::Void);
                    }
                    "typeof" => {
                        if let Expr::Ident(n) = &**arg {
                            if self.home(n).is_none() {
                                let name = self.name_idx(n);
                                self.emit(Op::TypeofName(name));
                                return Ok(());
                            }
                        }
                        self.expr(arg)?;
                        self.emit(Op::Typeof);
                    }
                    "delete" => return self.delete_expr(arg),
                    _ => return Err(Bail),
                }
                Ok(())
            }
            Expr::Await(arg) => {
                self.expr(arg)?;
                self.emit(Op::Await);
                Ok(())
            }
            Expr::Update { op, prefix, arg } => {
                let kind = match (*op, *prefix) {
                    ("++", true) => UpdKind::PreInc,
                    ("--", true) => UpdKind::PreDec,
                    ("++", false) => UpdKind::PostInc,
                    ("--", false) => UpdKind::PostDec,
                    _ => return Err(Bail),
                };
                self.update_target(arg, kind)
            }
            Expr::Assign { op, target, value } => self.assign(op, target, value),
            Expr::ToStr(inner) => {
                self.expr(inner)?;
                self.emit(Op::ToStr);
                Ok(())
            }
            Expr::OptionalChain(inner) => {
                let mut shorts = Vec::new();
                self.opt_chain(inner, &mut shorts)?;
                if shorts.is_empty() {
                    return Ok(()); // no optional link actually taken a short path
                }
                let done = self.emit(Op::Jump(0));
                for j in shorts {
                    self.patch(j);
                }
                self.emit(Op::Undef);
                self.patch(done);
                Ok(())
            }
            Expr::Call {
                callee,
                args,
                optional: false,
            } => {
                // Direct eval can see the activation — bail the function.
                if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    return Err(Bail);
                }
                match &**callee {
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                        self.expr(obj)?;
                        let i = self.name_idx(prop);
                        let c = self.new_cache();
                        self.emit(Op::GetMethod(i, c));
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) => {
                        self.expr(obj)?;
                        self.expr(index)?;
                        self.emit(Op::GetMethodElem);
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    Expr::Super => return Err(Bail),
                    Expr::Ident(name) if self.home(name).is_none() => {
                        // Free-name callee: resolved before the arguments (spec order), and a
                        // `with (obj) f()` hit supplies obj as `this`.
                        let i = self.name_idx(name);
                        let c = self.new_name_cache();
                        self.emit(Op::LoadNameForCall(i, c));
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    other => {
                        self.expr(other)?;
                        if self.call_args(args)? {
                            self.emit(Op::CallSpread(args.len() as u16));
                        } else {
                            self.emit(Op::Call(args.len() as u16));
                        }
                    }
                }
                Ok(())
            }
            Expr::New { callee, args } => {
                self.expr(callee)?;
                for a in args {
                    let ArrayElem::Item(a) = a else {
                        return Err(Bail);
                    };
                    self.expr(a)?;
                }
                self.emit(Op::New(args.len() as u16));
                Ok(())
            }
            Expr::Regex { body, flags } => {
                let body = self.name_idx(body);
                let flags = self.name_idx(flags);
                self.emit(Op::MakeRegExp(body, flags));
                Ok(())
            }
            Expr::Array(elems) => {
                for el in elems {
                    match el {
                        ArrayElem::Item(e) => self.expr(e)?,
                        _ => return Err(Bail),
                    }
                }
                self.emit(Op::MakeArray(elems.len() as u16));
                Ok(())
            }
            Expr::Object(props) => self.object_literal(props),
            other => {
                log_bail_node("expr", other, 60);
                Err(Bail)
            }
        }
    }

    /// `++`/`--` on a local slot, `obj.name`, or `obj[k]`; `kind` carries pre/post/discard.
    fn update_target(&mut self, arg: &Expr, kind: UpdKind) -> CResult {
        match arg {
            Expr::Paren(inner) => self.update_target(inner, kind),
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, false)) => {
                    self.emit(Op::UpdateLocal(slot, kind));
                    Ok(())
                }
                Some(Home::Env(false)) => {
                    let n = self.name_idx(name);
                    self.emit(Op::UpdateCap(n, kind));
                    Ok(())
                }
                Some(Home::Slot(_, true)) | Some(Home::Env(true)) => Err(Bail),
                None => {
                    let n = self.name_idx(name);
                    let c = self.new_name_cache();
                    self.emit(Op::UpdateNameCached(n, c, kind));
                    Ok(())
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::UpdateProp(i, c, kind));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::UpdateElem(kind));
                Ok(())
            }
            other => {
                log_bail_node("expr", other, 60);
                Err(Bail)
            }
        }
    }

    fn assign(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if matches!(op, "&&=" | "||=" | "??=") {
            log_bail("expr", "logical assignment");
            return Err(Bail);
        }
        match target {
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, is_const)) => {
                    if is_const {
                        return Err(Bail);
                    }
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreLocal(slot));
                    Ok(())
                }
                Some(Home::Env(is_const)) => {
                    if is_const {
                        return Err(Bail);
                    }
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadCap(n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreCap(n));
                    Ok(())
                }
                None => {
                    let i = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        // See the discarded-assignment path above for the stable-Reference proof.
                        let c = self.new_name_cache();
                        self.emit(Op::LoadName(i, c));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit_store_name(i);
                    Ok(())
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                if op == "=" {
                    self.expr(value)?;
                } else {
                    // Compound: base evaluated once (Dup), get before the RHS — Reference order.
                    self.emit(Op::Dup);
                    let cg = self.new_cache();
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                let c = self.new_cache();
                self.emit(Op::SetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref(), value]) {
                    self.expr(index)?;
                    if op == "=" {
                        self.expr(value)?;
                    } else {
                        self.emit(Op::ToPropKeyLocal(slot));
                        self.emit(Op::Dup);
                        self.emit(Op::GetElemLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::SetElemLocal(slot));
                    return Ok(());
                }
                self.expr(obj)?;
                self.expr(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    // Compound: coerce a side-effecting key once, then read-modify-write.
                    self.emit(Op::ToPropKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetElem);
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn emit_compound(&mut self, op: &str) -> CResult {
        let bop = match op {
            "+=" => Op::Add,
            "-=" => Op::Sub,
            "*=" => Op::Mul,
            "/=" => Op::Div,
            "%=" => Op::Mod,
            "&=" => Op::BitAnd,
            "|=" => Op::BitOr,
            "^=" => Op::BitXor,
            "<<=" => Op::Shl,
            ">>=" => Op::Shr,
            ">>>=" => Op::UShr,
            "**=" => {
                let i = self.name_idx("**");
                Op::GenBin(i)
            }
            _ => return Err(Bail),
        };
        self.emit(bop);
        Ok(())
    }
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

/// Execute a compiled function body. `env` is the root for free-name resolution — the *definition*
/// environment when called leanly (see `Interp::call_user_inner`), since a compiled body has no
/// observable activation. Parameters seed straight into slots; `this_val` is the already-bound
/// `this` (computed only when the body reads it). Synchronous bodies only — an async body runs
/// through [`VmCoro`], which drives [`run_vm`] and can suspend it.
///
/// The slot and operand-stack buffers come from a per-interpreter pool ([`Interp::vm_pool`]) so a
/// hot call tree does not allocate two `Vec`s per call.
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
    if let Some(s) = chunk.arguments_slot {
        slots[s as usize] = Value::Obj(i.make_compiled_arguments_object(args, &env));
    }
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
    loop {
        let outcome = match pending_throw.take() {
            Some(e) => Err(Abrupt::Throw(e)),
            None => run_vm(i, chunk, env, slots, stack, pc, this_val, handlers),
        };
        match outcome {
            Ok(step) => return Ok(step),
            Err(Abrupt::Throw(e)) => match handlers.pop() {
                Some(h) => {
                    stack.truncate(h.stack_depth);
                    stack.push(e);
                    *pc = h.catch_pc;
                }
                None => return Err(Abrupt::Throw(e)),
            },
            // Return/Break/Continue never escape a compiled body as an Abrupt; propagate defensively.
            Err(other) => return Err(other),
        }
    }
}

/// Run from `*pc` until the body returns (`Done`), suspends at an `await` (`Await`, async bodies
/// only), or throws (`Err(Abrupt::Throw)`, caught by [`drive_vm`]). Operates on borrowed state so an
/// async [`VmCoro`] can save it at a suspension and restore it on resume.
#[allow(clippy::too_many_arguments)]
fn run_vm(
    i: &mut Interp,
    chunk: &Chunk,
    env: &Env,
    slots: &mut [Value],
    stack: &mut Vec<Value>,
    pc: &mut usize,
    this_val: &Value,
    handlers: &mut Vec<Handler>,
) -> Result<VmStep, Abrupt> {
    macro_rules! pop {
        () => {
            stack.pop().expect("vm stack underflow")
        };
    }
    loop {
        let op = chunk.ops[*pc];
        *pc += 1;
        match op {
            Op::Const(k) => stack.push(chunk.consts[k as usize].clone()),
            Op::Undef => stack.push(Value::Undefined),
            Op::Dup => {
                let t = stack.last().expect("vm stack underflow").clone();
                stack.push(t);
            }
            Op::Pop => {
                pop!();
            }
            Op::LoadLocal(s) => {
                let v = slots[s as usize].clone();
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
            Op::StoreLocal(s) => slots[s as usize] = pop!(),
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
                        slots[idx] = Value::Num(new);
                        match kind {
                            UpdKind::PreInc | UpdKind::PreDec => stack.push(Value::Num(new)),
                            UpdKind::PostInc | UpdKind::PostDec => stack.push(Value::Num(old)),
                            UpdKind::IncDiscard | UpdKind::DecDiscard => {}
                        }
                    }
                    // BigInt updates stay BigInt (ToNumeric, not ToNumber) — never coerced to a
                    // Number and never thrown on like unary `+` would.
                    Value::BigInt(n) => {
                        let old = n.clone();
                        let one = crate::bigint::JsBigInt::from_u64(1);
                        let new = match kind {
                            UpdKind::PreInc | UpdKind::PostInc | UpdKind::IncDiscard => {
                                old.add(&one)
                            }
                            UpdKind::PreDec | UpdKind::PostDec | UpdKind::DecDiscard => {
                                old.sub(&one)
                            }
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
            Op::Tdz(s) => slots[s as usize] = Value::Empty,
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
                step_and_store(i, stack, kind, old, |_, v| {
                    if let Some(bd) = env.borrow_mut().vars.get_mut(name) {
                        bd.value = v;
                    }
                    Ok(())
                })?;
            }
            Op::UpdateName(n, kind) => {
                let name = &chunk.names[n as usize];
                let old = i.get_var(name, env)?;
                step_and_store(i, stack, kind, old, |i, v| i.assign_free_name(name, v, env))?;
            }
            Op::UpdateNameCached(n, c, kind) => {
                let name = &chunk.names[n as usize];
                let old = chunk.load_name_ic(i, env, n, c)?;
                step_and_store(i, stack, kind, old, |i, v| i.assign_free_name(name, v, env))?;
            }
            Op::MakeClosure(fidx, name_n) => {
                let v = i.make_function(chunk.funcs[fidx as usize].clone(), env.clone());
                if name_n != u32::MAX {
                    i.set_fn_name(&v, &chunk.names[name_n as usize]);
                }
                stack.push(v);
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
                let v = i.get_prop_ic(&obj, &chunk.names[n as usize], &chunk.caches[c as usize])?;
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
                if matches!(
                    stack.last().expect("vm stack underflow"),
                    Value::Undefined | Value::Null
                ) {
                    return Err(i.throw("TypeError", "cannot destructure null or undefined"));
                }
            }
            Op::DestructureArr(n) => {
                let v = pop!();
                if let Some(values) = array_destructure::try_dense(i, &v, n) {
                    stack.extend(values);
                } else {
                    let (it, nx) = i.get_iterator(&v)?;
                    let mut done = false;
                    for _ in 0..n {
                        if !done {
                            match i.iterator_step(&it, &nx)? {
                                Some(x) => {
                                    stack.push(x);
                                    continue;
                                }
                                None => done = true,
                            }
                        }
                        stack.push(Value::Undefined);
                    }
                    if !done {
                        i.iterator_close_normal(&it)?;
                    }
                }
            }
            Op::DeleteProp(n, strict) => {
                let base = pop!();
                let prop = &chunk.names[n as usize];
                if matches!(base, Value::Undefined | Value::Null) {
                    return Err(i.throw(
                        "TypeError",
                        format!("cannot delete property '{prop}' of null or undefined"),
                    ));
                }
                let v = i.delete_prop_with(base, prop, strict)?;
                stack.push(v);
            }
            Op::DeleteElem(strict) => {
                let idx = pop!();
                let base = pop!();
                let key = i.to_property_key(&idx)?;
                if matches!(base, Value::Undefined | Value::Null) {
                    return Err(i.throw(
                        "TypeError",
                        format!("cannot delete property '{key}' of null or undefined"),
                    ));
                }
                let v = i.delete_prop_with(base, &key, strict)?;
                stack.push(v);
            }
            Op::CallSpread(argc) | Op::CallSpreadThis(argc) => {
                let spread = pop!();
                let at = stack.len() - (argc as usize - 1);
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
                let v = i.call(callee, this, &args)?;
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
                if let (Value::Obj(o), Value::Num(n)) = (&obj, &key) {
                    if let Some(v) = i.fast_get_elem(o, *n) {
                        stack.push(v);
                        continue;
                    }
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
            Op::GetElemLocal(s) => {
                let key = pop!();
                if let (Value::Obj(o), Value::Num(n)) = (&slots[s as usize], &key) {
                    if let Some(v) = i.fast_get_elem(o, *n) {
                        stack.push(v);
                        continue;
                    }
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
                step_and_store(i, stack, kind, old, |i, v| {
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
                step_and_store(i, stack, kind, old, |i, v| i.set_member(&obj, &k, v))?;
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
            Op::Add => bin_num(i, &mut *stack, "+", |a, b| a + b)?,
            Op::Sub => bin_num(i, &mut *stack, "-", |a, b| a - b)?,
            Op::Mul => bin_num(i, &mut *stack, "*", |a, b| a * b)?,
            Op::Div => bin_num(i, &mut *stack, "/", |a, b| a / b)?,
            Op::Mod => bin_num(i, &mut *stack, "%", crate::eval::js_mod)?,
            Op::BitAnd => bin_i32(i, &mut *stack, "&", |a, b| a & b)?,
            Op::BitOr => bin_i32(i, &mut *stack, "|", |a, b| a | b)?,
            Op::BitXor => bin_i32(i, &mut *stack, "^", |a, b| a ^ b)?,
            Op::Shl => bin_i32(i, &mut *stack, "<<", |a, b| a.wrapping_shl(b as u32 & 31))?,
            Op::Shr => bin_i32(i, &mut *stack, ">>", |a, b| a >> (b as u32 & 31))?,
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
            Op::Lt => bin_cmp(i, &mut *stack, "<", |a, b| a < b)?,
            Op::Gt => bin_cmp(i, &mut *stack, ">", |a, b| a > b)?,
            Op::Le => bin_cmp(i, &mut *stack, "<=", |a, b| a <= b)?,
            Op::Ge => bin_cmp(i, &mut *stack, ">=", |a, b| a >= b)?,
            Op::EqEq => bin_cmp(i, &mut *stack, "==", |a, b| a == b)?,
            Op::NotEq => bin_cmp(i, &mut *stack, "!=", |a, b| a != b)?,
            Op::StrictEq => bin_cmp(i, &mut *stack, "===", |a, b| a == b)?,
            Op::StrictNotEq => bin_cmp(i, &mut *stack, "!==", |a, b| a != b)?,
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
            Op::Neg => {
                let a = pop!();
                match a {
                    Value::Num(n) => stack.push(Value::Num(-n)),
                    other => {
                        let v = i.eval_unary_vm("-", other)?;
                        stack.push(v);
                    }
                }
            }
            Op::Plus => {
                let a = pop!();
                match a {
                    Value::Num(n) => stack.push(Value::Num(n)),
                    other => {
                        let v = i.eval_unary_vm("+", other)?;
                        stack.push(v);
                    }
                }
            }
            Op::Not => {
                let a = pop!();
                stack.push(Value::Bool(!i.to_boolean(&a)));
            }
            Op::BitNot => {
                let a = pop!();
                match a {
                    Value::Num(n) => stack.push(Value::Num(!crate::eval::to_int32(n) as f64)),
                    other => {
                        let v = i.eval_unary_vm("~", other)?;
                        stack.push(v);
                    }
                }
            }
            Op::Typeof => {
                let a = pop!();
                let v = i.eval_unary_vm("typeof", a)?;
                stack.push(v);
            }
            Op::TypeofIs(kind, negated) => {
                let a = pop!();
                stack.push(Value::Bool((TypeofKind::of(i, &a) == kind) != negated));
            }
            Op::TypeofName(n) => {
                stack.push(i.typeof_name_vm(&chunk.names[n as usize], env)?);
            }
            Op::Void => {
                pop!();
                stack.push(Value::Undefined);
            }
            Op::Jump(t) => {
                // A backward jump is a loop turn: the same safe point the tree-walker runs at every
                // loop head, amortized, so an allocation-free `while (true) {}` still reaches the
                // collector and the embedder's interrupt.
                if (t as usize) < *pc {
                    i.gc_check_amortized()?;
                }
                *pc = t as usize
            }
            Op::JumpIfFalse(t) => {
                let a = pop!();
                if !i.to_boolean(&a) {
                    *pc = t as usize;
                }
            }
            Op::JumpIfNotCmp(kind, t) => {
                let b = pop!();
                let a = pop!();
                let yes = match (&a, &b) {
                    (Value::Num(x), Value::Num(y)) => kind.num(*x, *y),
                    _ => {
                        let v = i.binary(kind.name(), a, b)?;
                        i.to_boolean(&v)
                    }
                };
                if !yes {
                    *pc = t as usize;
                }
            }
            Op::ArithLL(kind, d, a, b) => {
                let v = match (&slots[a as usize], &slots[b as usize]) {
                    (Value::Num(x), Value::Num(y)) => Value::Num(kind.num(*x, *y)),
                    (x, y) => {
                        let (x, y) = (x.clone(), y.clone());
                        arith_slow(i, chunk, kind, x, y, a, Some(b))?
                    }
                };
                slots[d as usize] = v;
            }
            Op::ArithLK(kind, d, a, k) => {
                let v = match (&slots[a as usize], &chunk.consts[k as usize]) {
                    (Value::Num(x), Value::Num(y)) => Value::Num(kind.num(*x, *y)),
                    (x, y) => {
                        let (x, y) = (x.clone(), y.clone());
                        arith_slow(i, chunk, kind, x, y, a, None)?
                    }
                };
                slots[d as usize] = v;
            }
            Op::JumpIfNotCmpLL(kind, a, b, t) => {
                let yes = match (&slots[a as usize], &slots[b as usize]) {
                    (Value::Num(x), Value::Num(y)) => kind.num(*x, *y),
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
                let at = stack.len() - argc as usize;
                let callee = stack[at - 1].clone();
                let v = i.call(callee, Value::Undefined, &stack[at..])?;
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
                let at = stack.len() - argc as usize;
                let m = stack[at - 1].clone();
                let this = stack[at - 2].clone();
                let v = i.call(m, this, &stack[at..])?;
                stack.truncate(at - 2);
                stack.push(v);
            }
            Op::New(argc) => {
                let at = stack.len() - argc as usize;
                let callee = stack[at - 1].clone();
                let v = i.construct(callee, &stack[at..])?;
                stack.truncate(at - 1);
                stack.push(v);
            }
            Op::MakeRegExp(body, flags) => {
                stack.push(
                    i.make_regexp(&chunk.names[body as usize], &chunk.names[flags as usize])?,
                );
            }
            Op::MakeArray(n) => {
                let at = stack.len() - n as usize;
                let items: Vec<Value> = stack.split_off(at);
                stack.push(i.make_array(items));
            }
            Op::MakeObject(start, count, tidx) => {
                let at = stack.len() - count as usize;
                let values: Vec<Value> = stack.split_off(at);
                let keys = &chunk.names[start as usize..start as usize + count as usize];
                let v = if tidx != u32::MAX {
                    i.make_plain_object_templated(&chunk.obj_maps[tidx as usize], keys, values)
                } else {
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
                let (it, nx) = i.get_iterator(&v)?;
                stack.push(it);
                stack.push(nx);
            }
            Op::IterStepL(is, ns) => {
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
                let it = slots[s as usize].clone();
                i.iterator_close_normal(&it)?;
            }
            Op::IterAbortL(s) => {
                let exc = pop!();
                let it = slots[s as usize].clone();
                i.iterator_close(&it);
                return Err(Abrupt::Throw(exc));
            }
            Op::Throw => {
                let v = pop!();
                return Err(Abrupt::Throw(v));
            }
            Op::Return => return Ok(VmStep::Done(pop!())),
            Op::ReturnUndef => return Ok(VmStep::Done(Value::Undefined)),
            Op::Await => return Ok(VmStep::Await(pop!())),
            Op::PushHandler(catch_pc) => handlers.push(Handler {
                catch_pc: catch_pc as usize,
                stack_depth: stack.len(),
            }),
            Op::PopHandler => {
                handlers.pop();
            }
        }
    }
}

/// An async function body running on the bytecode VM, suspendable at each `await` without an OS
/// thread. It presents the same `resume(&mut Interp, Resume) -> Suspend` shape as the thread-backed
/// coroutine, so the promise driver (`Interp::drive_async`) treats both uniformly — the only cost
/// per await is now a couple of `Vec` swaps instead of a thread handoff.
pub struct VmCoro {
    chunk: Rc<Chunk>,
    env: Env,
    this_val: Value,
    slots: Vec<Value>,
    stack: Vec<Value>,
    pc: usize,
    /// The `try` handler stack, saved across suspensions so a rejected `await` inside a `try` still
    /// lands in its `catch`.
    handlers: Vec<Handler>,
    pub done: bool,
    pub started: bool,
}

impl VmCoro {
    /// Build an async coroutine for `chunk` with params seeded from `args`, parked before its first
    /// step (run on the first `resume`).
    pub fn new(i: &Interp, chunk: Rc<Chunk>, env: Env, this_val: Value, args: &[Value]) -> VmCoro {
        let env = chunk.make_run_env(i, &env, &this_val, args);
        let mut slots = vec![Value::Undefined; chunk.n_slots];
        for (k, a) in args.iter().take(chunk.n_params).enumerate() {
            slots[k] = a.clone();
        }
        for &s in &chunk.var_force_resets {
            slots[s as usize] = Value::Undefined;
        }
        VmCoro {
            chunk,
            env,
            this_val,
            slots,
            stack: Vec::with_capacity(16),
            pc: 0,
            handlers: Vec::new(),
            done: false,
            started: false,
        }
    }

    /// Drive one step: run to the next `await` (`Suspend::Await`), to completion (`Done`), or to an
    /// uncaught throw (`Throw`). A `Resume::Throw` (rejected await) is injected at the await point so
    /// an enclosing `try`/`catch` in the body can catch it; only an uncaught one rejects the function.
    pub fn resume(
        &mut self,
        i: &mut Interp,
        signal: crate::coroutine::Resume,
    ) -> crate::coroutine::Suspend {
        use crate::coroutine::{Resume, Suspend};
        if self.done {
            return Suspend::Done(Value::Undefined);
        }
        let pending_throw = match signal {
            Resume::Next(v) => {
                if self.started {
                    self.stack.push(v); // the settled value of the await we parked at
                }
                None
            }
            // A rejected await: re-enter the VM throwing `e` at the suspension point.
            Resume::Throw(e) if self.started => Some(e),
            Resume::Throw(e) => {
                self.done = true;
                return Suspend::Throw(e);
            }
            Resume::Return(v) => {
                self.done = true;
                return Suspend::Done(v);
            }
        };
        self.started = true;
        match drive_vm(
            i,
            &self.chunk,
            &self.env,
            &mut self.slots,
            &mut self.stack,
            &mut self.pc,
            &self.this_val,
            &mut self.handlers,
            pending_throw,
        ) {
            Ok(VmStep::Await(a)) => Suspend::Await(a),
            Ok(VmStep::Done(v)) => {
                self.done = true;
                Suspend::Done(v)
            }
            Err(Abrupt::Throw(e)) => {
                self.done = true;
                Suspend::Throw(e)
            }
            // Return/Break/Continue can't escape a function body; treat defensively as completion.
            Err(_) => {
                self.done = true;
                Suspend::Done(Value::Undefined)
            }
        }
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
            let one = crate::bigint::JsBigInt::from_u64(1);
            let new = if inc { n.add(&one) } else { n.sub(&one) };
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
fn step_and_store(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    kind: UpdKind,
    old: Value,
    set: impl FnOnce(&mut Interp, Value) -> Result<(), Abrupt>,
) -> Result<(), Abrupt> {
    if let Some(v) = step_value(i, kind, old, set)? {
        stack.push(v);
    }
    Ok(())
}

#[inline]
fn bin_num(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    op: &'static str,
    f: impl Fn(f64, f64) -> f64,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        stack.push(Value::Num(f(*x, *y)));
        return Ok(());
    }
    let v = i.binary(op, a, b)?;
    stack.push(v);
    Ok(())
}

#[inline]
fn bin_i32(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    op: &'static str,
    f: impl Fn(i32, i32) -> i32,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        stack.push(Value::Num(
            f(crate::eval::to_int32(*x), crate::eval::to_int32(*y)) as f64,
        ));
        return Ok(());
    }
    let v = i.binary(op, a, b)?;
    stack.push(v);
    Ok(())
}

#[inline]
fn bin_cmp(
    i: &mut Interp,
    stack: &mut Vec<Value>,
    op: &'static str,
    f: impl Fn(f64, f64) -> bool,
) -> Result<(), Abrupt> {
    let b = stack.pop().expect("vm stack underflow");
    let a = stack.pop().expect("vm stack underflow");
    if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        stack.push(Value::Bool(f(*x, *y)));
        return Ok(());
    }
    let v = i.binary(op, a, b)?;
    stack.push(v);
    Ok(())
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
    /// Depth-0 cache fill: the name resolves directly in `env` as a plain initialized binding —
    /// no `with` object on the scope, no live import redirect. When `env` *is* the global scope
    /// and misses, an own data property of the ordinary global object fills the global mode
    /// instead. Returns the value on success; `None` = not cacheable at this site (the caller
    /// runs the interpreter's full walk, uncached).
    fn name_ic_fill(&self, i: &Interp, env: &Env, n: u32, c: u32) -> Option<Value> {
        {
            let b = env.borrow();
            if b.with_obj.is_some() {
                return None;
            }
            if let Some(bd) = b.vars.get(&*self.names[n as usize]) {
                if !bd.initialized || bd.import_ref.is_some() {
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
                    if pb.with_obj.is_none() {
                        if let Some(bd) = pb.vars.get(&*self.names[n as usize]) {
                            if bd.initialized && bd.import_ref.is_none() {
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
        if !i.ordinary_get_ptr(Gc::as_ptr(&i.global) as usize) {
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
                if bd.initialized && bd.mutable && bd.import_ref.is_none() {
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
                        if bd.initialized && bd.mutable && bd.import_ref.is_none() {
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
        i.assign_free_name(&self.names[n as usize], value, env)?;
        let _ = self.name_ic_fill(i, env, n, c);
        Ok(())
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
            Op::New(argc) => (*argc as usize + 1, 1),
            Op::MakeRegExp(..) => (0, 1),
            Op::MakeArray(n) => (*n as usize, 1),
            Op::MakeObject(_, count, _) => (*count as usize, 1),
            Op::Throw | Op::Return => (1, 0),
            Op::ReturnUndef => (0, 0),
            Op::Await => (1, 1),
            Op::PushHandler(_) | Op::PopHandler => (0, 0),
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
    for op in out.iter_mut() {
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
    *ops = out;
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
