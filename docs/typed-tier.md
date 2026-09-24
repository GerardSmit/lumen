# Typed tier: using TypeScript and JSDoc types to make code faster

Status: design, nothing implemented. Companion to [jit.md](jit.md) and
[jit-optimizations.md](jit-optimizations.md). Read the "Where lumen is today" table in the
latter first. This document assumes the tiers described there: tree-walker, bytecode VM,
template/whole-function JIT with `Kind::{Num, Bool, Boxed}`, and the AOT blob of
`crates/lumen/src/precompiled.rs`.

The plan has three stages. Each stage is useful without the next one.

| Stage | What types are used for | Trust | Works for |
|---|---|---|---|
| 1. Hints | Initial object layouts, JIT kinds at the first call, earlier tier-up | None. Every fact stays guarded exactly as feedback would be | Any `.ts`, and any `.js` with JSDoc, however partial |
| 2. Sound subset | A local checker marks functions *sound* | Only inside sound functions, and only after boundary checks | Fully annotated functions that avoid the escape hatches in section 4 |
| 3. Typed native code | Sound functions compile with no internal type guards, by the JIT and ahead of time | As stage 2 | JIT targets, and AOT for iOS (no runtime codegen) and Puppeteer |

**Non-negotiable invariant.** Types never change what a program does. A program behaves
exactly as the same program with its types stripped. If a type turns out to be wrong
(because `any` leaked in, a `.d.ts` lied, or untyped code mutated a typed object), lumen does
not throw. The affected call continues in the bytecode VM with ordinary JS semantics. This
is where lumen deliberately departs from Static Hermes (section 2). An opt-in strict mode that
throws, like Static Hermes, is possible later but is not part of this plan.

---

## 1. Where lumen is today (grounding)

- **TypeScript is erased before the engine sees it.** `.ts`/`.mts`/`.cts` files go through
  `crates/lumen-node/src/js/typescript_strip.js` (called from `module.js`, lines 602 and 605;
  also exposed as `node:module.stripTypeScriptTypes`). It is a location-preserving blanker:
  type syntax becomes spaces, and newlines are kept, so every byte offset in the stripped JS
  equals the offset in the `.ts` source. It rejects `enum` and `namespace` declarations as
  syntax that needs transformation, as Node's strip mode does. The types are thrown away.
- **`crates/lumen-typescript`** (619 lines) is a type-expression parser (`parse_type_expression`)
  plus a structural `is_assignable` over `Type::{Any, Unknown, …, Union, Object, Function}`.
  It cannot parse declarations, statements or expressions, and nothing depends on it yet.
  Its assignability is TS-flavoured: `Any` is assignable both ways, and arrays are covariant
  (`(Array(s), Array(t)) => is_assignable(s, t)`). Both are unsound, and section 4 has to
  change them.
- **The JIT** (`crates/lumen/src/bytecode/jit/`) has two parts. A loop tier enters by OSR at
  loop headers. A whole-function tier (`FuncState`, `on_entry`, `compile_fn`) looks at
  compiling a function every `CALL_MASK + 1` = 1024 calls, and at the first call under
  `LUMEN_JIT_EAGER`. Local kinds are chosen from the **slot values at compile time**
  (`Kind::of(&Value)`) and checked by entry guards. A failed guard returns `EXIT_ENTRY_FAIL`
  before anything has happened, and the interpreter runs the call. Repeated failures widen
  slots to `Boxed` (`widen`, `MAX_ENTRY_FAILS`, `DEOPT_LIMIT`). Other exits (`EXIT_RESUME`,
  `EXIT_THROW`) materialize the operand stack, and the interpreter resumes at that pc.
  `call.rs` already makes direct native-to-native calls through `ChunkJit::dentry`, and
  `inline.rs` already does a tiny static type inference for pure numeric callees. **There is
  no Int32 kind.**
- **Values** are 16 bytes: a tag byte (`TAG_UNDEFINED..TAG_OBJ` = 0..8), a bool byte at 1, and
  the payload at 8 (`value.rs`). Values are refcounted, with a cycle GC on top (`docs/gc.md`).
- **Objects.** `Props` keeps keys in a shared `Shape` (a transition tree, and owned past 32 keys)
  and values in `entries`, in shape order. An inline-cache hit is a (shape, slot) pair. Small
  objects keep up to `INLINE_PROPS` = 4 property slots in the same heap block
  (`value/heap.rs`, `SlotClass::Inline`). Object-literal sites clone a pre-shaped template
  (`Chunk::obj_maps`); class constructors do not have one. Arrays carry an optional **f64
  mirror** (`Props::mirror`, `MIRROR_OK | MIRROR_NO_HOLES | MIRROR_ALL_I32`), so an all-number
  array already has flat `Vec<f64>` storage and an O(1) "every element is a number" flag.
- **Invalidation.** `proto_epoch` is a global counter bumped on prototype-structure changes,
  `setPrototypeOf` and attribute rewrites. There are no per-code dependencies yet (T6 in
  jit-optimizations.md).
- **AOT** (`lumen-aot` `include_js!`, `precompiled.rs`). The blob has `SEC_MANIFEST`, `SEC_AST`,
  `SEC_BYTECODE` (chunks keyed by *function index*) and `SEC_SOURCE`. It has no native
  sections. `layout_fp` is reserved for layout-dependent sections. The bytecode codec refuses
  chunks that contain class definitions ("class definitions are not carried by the codec"),
  so class-heavy code currently falls back to compiling at run time. `include_js!` does not
  accept `.ts`. iOS is interpreter-only.

## 2. What others learned (research summary)

- **Static Hermes** (Meta, Tzvetan Mikov) compiles soundly typed Flow, and experimentally TS
  lowered to the Flow AST, to native code via LLVM, with a C emitter kept for development.
  Its `-typed` mode *changes JS semantics*, and this is opt-in: values that are `any` entering
  typed code get a runtime-checked cast that **throws** on mismatch, and an out-of-bounds read
  of `number[]` throws `RangeError` instead of returning `undefined`. Typed property access is
  a direct offset load, and typed array access is a bounds check plus a load. Reported nbody
  results: 5511 → 565 ms (RN EU 2023); 1300 ms untyped → 350 ms typed → 120 ms with inlining
  and object elision (Chain React 2024). The same benchmark compiled **untyped** to native was
  *slower* than the Hermes interpreter (2388 vs 2086 ms). Mikov's own conclusion: untyped AOT
  "isn't a performance improvement over a high tier JIT". Types are what pay, not native code
  alone. As of late 2024 there were no integer types, and legacy JS classes were not supported
  in typed mode. [slides](https://speakerdeck.com/tmikov2023/optimizing-with-static-hermes-chain-react-2024),
  [blog](https://tmikov.blogspot.com/2023/09/how-to-speed-up-micro-benchmark-300x.html),
  [discussion](https://github.com/facebook/hermes/discussions/1137).
- **Static TypeScript** (MakeCode, MPLR '19) uses **nominal classes** with vtables and static
  field layout. Casts emit no code; soundness is enforced at member access with a nominal
  subtype check (`this.f` 6.5 cycles, `(x as C).f` 23, interface access 52, dynamic map 137).
  Dropping nominal layout cost +102% to +143% on richards. Disabling all the subtype checks
  saved only 3–17%. Checks are cheap; layout is what matters.
- **AssemblyScript** has only machine types (`i32`, `f64`, …), no `any`, and no unions of
  primitives. Its own FAQ says ordinary TS "does not magically become faster". It is a
  different language, not a model for running existing TS.
- **StrongScript** (ECOOP '15) has three kinds of type: `any`, optional `C` (TS-like, never
  adds errors) and concrete `!C` (guaranteed). Classes become nominal because "efficient
  property access code for structural subtyping is not a solved problem". Up to 22% speedup
  in a modified V8.
  **Safe TypeScript** (POPL '15) uses RTTI plus differential subtyping and reported 15%
  overhead on 90 KLOC.
- **Boundary cost.** Takikawa et al. (POPL '16), "Is sound gradual typing dead?", measured
  wrapper/contract-based sound gradual typing in Typed Racket: mixed configurations up to
  about 100x slower (suffixtree 88x with one module typed). The fixes in later work:
  - **Nominal, first-order checks with no wrappers.** In Nom (OOPSLA '17), sieve ran 20.2 /
    20.0 / 20.4 s across mixed configurations, against 503 / 1332 s in Typed Racket, and got
    *faster* as types were added (15.8 s fully typed).
  - **Transient checks at use sites** (Reticulated Python). These cost 2–6x on CPython, but
    are "almost free" under a JIT (ECOOP '19, PyPy).
  - **Concrete plus transient in production.** Instagram's Static Python ran +3.7% req/s.

  The design below takes these lessons as rules:
  - Never allocate a wrapper or proxy.
  - Check only O(1)-checkable facts at boundaries: a tag, or a class id via the shape.
  - Check containers lazily at element use.
  - Keep classes nominal.
- **Engines.** The TC39 type-annotations proposal (Stage 1) says outright that improving
  performance is a non-goal. Its authors know of no experiment that "meaningfully beat[s]
  dynamic type-driven JIT optimization". V8's SoundScript/StrongMode (2015) tried sound
  TS-based typing for optimization and was abandoned. I found no official V8/JSC statement
  that "types don't help because feedback already has them"; treat that as folklore.

  The honest reading: for a *mature speculative JIT on a desktop*, types-as-hints buy
  warm-up and little else. The sound, AOT-compiled case, as in Static Hermes, is where
  large wins are documented. That case matters most to lumen: iOS has no JIT, and lumen's
  JIT is still about 10x behind V8 (jit.md).

---

## 3. Architecture: how type information flows

```
 .ts / .mts / .cts / .js(+JSDoc)
        │
        ▼
 lumen-typescript front end (Rust, dependency-free)
   ├─ strip: location-preserving JS text (replaces typescript_strip.js; same bytes out)
   ├─ checker: per-function facts + sound verdict + reasons
   └─ TypeTable { file_id, fns: offset → FnTypes, classes: [ClassLayout] }
        │                       (keyed by the function's start byte offset, which the
        ▼                        blanking strip preserves: FnSource::Range.start)
 lumen parser/compiler (unchanged parse of the stripped text)
   └─ bytecode compile: Function(offset) ─lookup─▶ FnTypes ─▶ Chunk::types (side table)
        │
        ├─▶ JIT: stage 1 kinds/first-call compile; stage 3 sound compile (two entries)
        └─▶ AOT: SEC_TYPES (+ class layouts) and SEC_NATIVE (per target) in the blob
```

### 3.1 Front end (`crates/lumen-typescript`)

> **Layout update:** the `lumen-typescript` crate is gone. TypeScript *syntax* is parsed by the
> engine parser itself (`crates/lumen/src/parser/ts.rs`, always compiled; strips in place and
> records a per-source side table of type/JSDoc spans keyed by file byte offset). The checker
> (`types`/`check`/`table`/`walk`/`jsdoc`) is `lumen::typescript`, behind cargo feature `typed`
> (default on); `lumen::typescript::type_table(&FnSource::Range.src)` returns the cached
> `TypeTable` for a parsed source, and `fn_at(range.start)` works for ES and CommonJS modules
> alike (CommonJS compiles header-free, so no offset shift). `tsconfig` lives in lumen-runtime,
> and the `lumen-typed` binary is `lumen-cli typed`.

- It becomes a real parser for TS syntax: declarations, statements, expressions, classes,
  generics, and JSDoc tags in comments. It builds its **own** AST, and the engine's parser is
  never taught TS. Keeping the engine type-agnostic is the crate's stated contract (its
  `lib.rs` header), and the location-preserving strip makes that cheap: the checker and the
  engine agree on byte offsets without sharing an AST.
- The Rust stripper must produce byte-identical output to `typescript_strip.js`. That is
  verified by differential tests over a corpus; see section 9. The JS stripper stays as
  `node:module.stripTypeScriptTypes` until the Rust one is proven, then becomes a thin
  binding to it.
- Output is a `TypeTable`:

```rust
pub struct TypeTable {
    pub fns: Vec<(u32 /*start offset*/, FnTypes)>,   // sorted by offset
    pub classes: Vec<ClassLayout>,
    pub report: Vec<SoundnessNote>,                   // why not sound (diagnostics)
}
pub struct FnTypes {
    pub params: Vec<TKind>, pub this: TKind, pub ret: TKind,
    pub locals: Vec<(u32 /*decl offset*/, TKind)>,    // mapped to slots by the compiler
    pub sites: Vec<(u32 /*expr offset*/, SiteFact)>,  // per property/call/element site
    pub sound: bool,
}
pub enum TKind {            // the engine-facing projection of a TS type
    Any,                    // no information (stage 1 treats as absent)
    Num, Bool, Str, BigInt, Undef, Null, Sym,
    Tags(u16),              // union of the above as a tag bitmask (e.g. number|undefined)
    Class(u32),             // nominal: index into classes, `| null` via NullableClass
    NullableClass(u32),
    NumArray,               // number[] / readonly number[] (flat f64 storage)
    Array(Box<TKind>),      // T[] for other T: elements checked at use
    Func(u32),              // a signature id (params/ret), for typed-entry dispatch
    Object,                 // structural object/interface type: layout unknown
}
pub struct ClassLayout {
    pub name: String, pub parent: Option<u32>,
    pub fields: Vec<(String, TKind, /*readonly*/ bool)>, // own fields, definition order
    pub methods: Vec<(String, u32 /*fn offset*/)>,
    pub sound: bool,
}
pub enum SiteFact { Field { class: u32, index: u16 }, Elem(TKind), Callee(u32 /*fn offset*/),
                    Check(TKind) /* a boundary check the checker inserted */ }
```

`TKind` is deliberately coarse. It records what the engine can *represent* or *check in
O(1)*. Everything richer (literal types, template-literal types, mapped/conditional types,
generics) projects to `Any`, `Object` or a tag set. The checker reasons with the full `Type`;
the engine only ever sees `TKind`.

### 3.2 Engine side

- **Attach.** `Engine::attach_types(source_key, Rc<TypeTable>)` is called by the module
  loader (`module.js`'s `.ts` path, ESM loading, `include_js!`) before the source is compiled.
  The bytecode compiler looks up `Function`'s `FnSource::Range.start` (the class's start for
  field layouts). A miss means no types, and nothing changes. A precompiled blob decodes
  against the empty source, so there it uses the function index (section 7).
- **`Chunk::types: Option<Box<ChunkTypes>>`** is a new field. It is `None` for untyped code,
  so there is no per-chunk cost.

```rust
pub(crate) struct ChunkTypes {
    pub sound: bool,
    pub entry: Vec<Check>,          // one per param, then `this`; the entry-guard program
    pub ret: TKind,
    pub slot_kinds: Vec<TKind>,     // per slot (params + locals the compiler mapped)
    pub sites: Vec<(u32 /*pc*/, SiteFact)>,  // translated from expr offsets at compile time
    pub sig: u32,                   // signature id for typed-entry matching
}
```

- **Bytecode is untouched.** The interpreter never reads `ChunkTypes`: bytecode always runs
  with plain JS semantics and is the universal fallback. That is what makes "failed check →
  run bytecode" correct by construction.
- **Class layouts** become `TypedClass` records in the realm (id, parent, field list, and the
  pre-built typed shape; see section 5.3). `MakeClass` of a class with a `ClassLayout`
  registers one.

### 3.3 Tree-walker

A function with `ChunkTypes` skips the tree-walker and compiles to bytecode at its first call.
Its types already give the JIT what warm-up would have given it.

---

## 4. The sound subset (stage 2)

### 4.1 What "sound" means here

A function is **sound** when, for every execution that *enters its native body* (after the
entry checks pass):
- every value in a slot or expression of static type `T` satisfies `T`'s runtime projection,
  so the engine may drop guards on it; and
- every fact about the outside world the code relies on either is re-validated after any
  point where untyped code can run, or is covered by a watchpoint.

Soundness is per function. A module can be half sound. The checker is **local**: it checks
one fully annotated function at a time against declared signatures. It is **not** tsc, and it
never reports an error that stops a program. Anything it does not understand makes the
function "not sound (reason)", and the function runs as normal JS with stage-1 hints. tsc
stays the user's type checker; lumen's checker is an optimizer's proof obligation.

### 4.2 TypeScript constructs that are unsound, and the subset rule for each

| # | Construct | Why it is unsound | Subset rule |
|---|---|---|---|
| 1 | `any`: explicit, implicit (`noImplicitAny` off), or from `JSON.parse`, `catch (e)`, `Array.isArray` narrowing, `Function`, untyped imports | Assignable both ways | A function whose params, return, locals or any expression type contains `any` is not sound. `unknown` is fine: it is a sound top type (`Boxed`) |
| 2 | `x as T`, `<T>x` (checked by TS only for "sufficient overlap"; `as unknown as T` has no check at all) | Unchecked downcast | Allowed when `T` projects to a checkable `TKind` (primitive, tag set, class, `number[]`, `T[]`): the engine emits a **check** (section 6.2). Any other target makes the function not sound. `as const` is fine |
| 3 | Non-null `x!` | Unchecked | Compiled as a check (not null/undefined) |
| 4 | Definite assignment: `let x!: T` and field `x!: T` | Unchecked initialization | The type is widened to `T \| undefined` |
| 5 | User type predicates `x is T`, `asserts x is T` | The predicate body is not checked | Narrowing from such a call inserts a check of `T` at the narrowing point |
| 6 | `declare` (ambient vars, `declare` fields, `declare global`, `.d.ts`, `declare module`) | Descriptions, not verified | Values from declared-only bindings are *untrusted*: read as `unknown`, then checked where they flow into a typed slot. This also covers lib typings, except engine intrinsics lumen implements itself (§4.4) |
| 7 | Element and index access without `noUncheckedIndexedAccess` (`a[i]: T`, `{[k: string]: T}`) | Out of bounds or a missing key gives `undefined` | Array reads are bounds-checked. Out of bounds **exits to bytecode**, where JS then yields `undefined`. Index-signature reads are checked at use. With `noUncheckedIndexedAccess` the checker sees `T \| undefined` and needs no exit |
| 8 | Mutable array covariance (`Dog[]` → `Animal[]`) | Writes through the alias break the original | Mutable arrays are **invariant**. `readonly T[]` / `ReadonlyArray<T>` is covariant, and writes to it are rejected |
| 9 | Method parameter bivariance (method shorthand, even under `strictFunctionTypes`); optional vs. rest parameter interchange | Callee gets the wrong argument type | Every function type is contravariant in params. Arity must match; optional params count as `T \| undefined` |
| 10 | Structural width plus optional properties (`{a: string}` → `{}` → `{a?: number}`), without `exactOptionalPropertyTypes` | A property read gives an unexpected type | Structural object and interface types project to `TKind::Object`: no layout, and reads are checked at use. Only **classes** get layouts (nominal, as in STS, StrongScript and Nom) |
| 11 | Property narrowing survives calls (`if (typeof o.x === "number") { f(); o.x }`) | `f` may write `o.x` | Narrowing applies only to locals that are not written by any closure. Property narrowing is dropped at any call or any write that may alias |
| 12 | Class field / method override changes type (a subclass declares `x: 1` where the base has `x: number`) | Base code writes a value the subclass does not allow | Fields are invariant in subclasses (a `readonly` field may be covariant). Method overrides are checked contravariant/covariant |
| 13 | `this` in extracted methods and callbacks (`const m = p.m; m()`); `this` parameters | `this` can be anything | Sound methods check `this` at the generic entry like any parameter |
| 14 | Uninitialized fields: `strictPropertyInitialization` off, or `this` escaping in the constructor before all fields are set (calling a method that reads one) | A field reads as `undefined` | A field is `T` only if it has an initializer, or is assigned on every constructor path before any use of `this` other than field stores. Otherwise it is `T \| undefined` |
| 15 | Numeric enums accept any number; `const enum` / `enum` need emit | — | The strip rejects enums today anyway. Numeric enum types are `number` |
| 16 | Overloads (the implementation signature is only loosely checked against them) | — | Only the implementation signature is used |
| 17 | `// @ts-ignore`, `@ts-expect-error`, `@ts-nocheck` | Suppress errors | Any of these inside a function (or `@ts-nocheck` in the file) makes it not sound |
| 18 | `strictNullChecks` off | `null`/`undefined` belong to every type | The whole file is hints-only (stage 1) |
| 19 | `in` / `instanceof` narrowing over structural types; discriminated unions over interfaces | The value may have other shapes | Narrowing to a *class* by `instanceof` is sound (nominal check). Narrowing a structural type yields `Object` |
| 20 | Generics (`T` erased, instantiated with anything, including `any` from outside) | — | Type parameters are `Boxed`/`unknown` inside the body. A value of type `T` is checked when it becomes a concrete type. No specialization in v1 |
| 21 | `eval`, `with`, `arguments`, `Function.prototype.call/apply/bind` on sound functions, `new Function` | Dynamic scope or arity | `eval`, `with`, `arguments` inside a function: not sound. Calls *into* a sound function through `call/apply/bind` use the generic (checked) entry |
| 22 | Getters/setters and `Object.defineProperty` on typed instances; `delete`; `Object.setPrototypeOf` / `__proto__` writes; prototype method reassignment | Layout or behaviour changes under the code | Not static. Handled at run time by typed shapes and watchpoints (§5.3) |
| 23 | Implicit coercions (`obj + ""`, template literals with objects, `==` with objects, `valueOf`/`toString`/`Symbol.toPrimitive`) | Run arbitrary JS | Allowed, but treated as calls to untyped code (§6.3) |
| 24 | Async functions and generators | Suspension points | Not sound in v1 (hints only). Their bodies suspend across untyped code by construction |

### 4.3 Checker scope

- **In scope.** Primitive types, unions of primitives, `null`/`undefined`, literal types
  (widened), classes (fields, methods, `extends`, `super`, `static`, `#private`), functions and
  closures (arrows included), arrays and tuples (tuples project to `Array`), local type
  inference from initializers, control-flow narrowing of locals (`typeof`, `===`, truthiness,
  `instanceof` of classes), type aliases, interfaces (structural, `Object`), `readonly`, and
  imports between modules checked in the same compilation.
- **Out of scope** (projects to `Any`/`Object`, never an error): conditional and mapped
  types, `infer`, template literal types, declaration merging, `namespace`, decorators' type
  effects, overload resolution beyond implementation signatures, the full `lib.d.ts`
  semantics, and variance annotations.
- **The fallback "unsound, runs as normal JS" covers everything else.** The function gets
  stage-1 hints from whatever annotations exist, no trust, and the full speculative JIT.

### 4.4 Engine intrinsics as trusted signatures

`Math.*`, `Number.isInteger`, `String.prototype.length/charCodeAt`, `Array.prototype.length`,
`push`/`pop` on arrays, and typed-array element access have exact types because lumen
implements them. The checker treats these as trusted signatures, and the compiled code
guards their identity with a watchpoint (the existing `MathGuard` becomes a watchpoint, T6).
Any other `lib.d.ts` or `.d.ts` binding is untrusted (row 6).

### 4.5 JSDoc

JSDoc types (`@param {number} x`, `@returns`, `@type`, `@typedef`, `@template`, `@this`,
`/** @type {number} */ this.x = 0` in constructors, and JSDoc on class fields) map onto the
same `Type`. Both stage 1 and stage 2 apply. Caveats:

- **A JSDoc cast** `/** @type {X} */ (y)` is an unchecked assertion, like `as` (row 2): it
  becomes a check or makes the function not sound.
- **`{*}`, `{?}`, `{Object}`, `{Function}`, `{Array}`** (without a type argument) are `any`.
- **Opt-in.** A file is considered only with `// @ts-check` or `checkJs: true`. Otherwise its
  JSDoc is documentation and possibly stale: stage 1 may use it (guarded, so harmless), and
  stage 2 must not.
- **Attachment is heuristic** (which comment belongs to which node). The front end follows
  tsc's rules: the nearest preceding `/** */` of the declaration statement.
- **Fields.** JS has no field declaration syntax outside class fields, so a layout comes only
  from class fields or from constructor `this.x =` assignments with JSDoc.

Verdict: JSDoc code **can** be sound under the same rules. Expect a lower yield in practice:
casts, `{Object}`, and missing return types are common in JSDoc codebases, and the
`--typed-report` diagnostics are what make the yield visible.

### 4.6 tsconfig

The front end reads the nearest `tsconfig.json` (`extends` followed, JSONC tolerated):

| Option | Effect |
|---|---|
| `strict` / `strictNullChecks` off | The file is hints-only (row 18) |
| `noImplicitAny` off | Unannotated params are `any` → not sound (same as with it on: the subset needs annotations) |
| `strictFunctionTypes` | Ignored: the subset is always contravariant (row 9) |
| `strictPropertyInitialization` | Ignored: the subset does its own definite-assignment analysis (row 14) |
| `noUncheckedIndexedAccess` | Honoured: removes the out-of-bounds exit on reads the code already handles (row 7) |
| `exactOptionalPropertyTypes` | Honoured for `Object` checks |
| `useDefineForClassFields` | Must match the strip's runtime semantics (`[[Define]]` of fields, which is what blanking produces). If it is `false`, class layouts are hints-only |
| `checkJs` / `allowJs` | Enable JSDoc (4.5) |
| `target`, `module`, `paths` | `paths` is used for type-only resolution; the others are ignored |

No tsconfig means lumen's defaults: strict on, and `noUncheckedIndexedAccess` off.

### 4.7 Diagnostics

- `lumen --typed-report[=json] app.ts` (env `LUMEN_TYPED_REPORT=1`) prints, per function, its
  verdict and the **first** reason with a location:
  ```
  src/vec.ts:12:3  dot(a, b)         sound
  src/vec.ts:30:3  norm(v)           not sound: `v.data` is `any` (src/vec.ts:31:17)
  src/io.ts:4:1    load(path)        not sound: async function (hints only)
  src/geo.ts:9:5   Point             layout: 3 fields (x: number, y: number, tag: string)
  ```
- A run-time section (`LUMEN_TYPED_REPORT=runtime`) prints entry-check failure counts and
  demotions (§6.5). A function that is sound on paper but whose callers keep passing the wrong
  types is the most useful thing to show a user.
- The modes are `--typed=off|hints|sound` (default `sound` once stage 3 ships; `hints` until
  then), plus `LUMEN_TYPED_STRESS` (section 9).

---

## 5. Stage 1: types as hints

Everything in this stage is a **guess fed to machinery that already guards**. A wrong type
costs one guard failure and one widening, exactly as a wrong feedback observation does today.

### 5.1 Compile at first call with declared kinds

`compile_fn` currently takes kinds from `slots` at the moment of compilation. With
`ChunkTypes`:
- `entry_due` returns true at the first call (the same path as `LUMEN_JIT_EAGER`, but per chunk).
- `build_fn` takes each param's kind from `slot_kinds` (`Num` → `Kind::Num`, `Bool` →
  `Kind::Bool`, anything else → `Boxed`). If a declared kind contradicts the actual first
  argument, the observed value wins (never compile code that fails on its first entry).
- Locals declared `number`/`boolean` start unboxed. The existing entry guards and `widen`
  logic stay unchanged.
- Property sites whose `SiteFact::Field` names a class field get a *predicted* (shape, slot)
  as soon as that class's shape exists. It is still guarded by the shape check.

Gain: removes the 1024-call warm-up and lets short-running programs (CLIs, Puppeteer scripts,
tests) reach native code. There is no steady-state gain over well-fed feedback.

### 5.2 Pre-sized instances

Instance shape is decided by `ClassLayout.fields` in definition order. Base fields come first,
because JS defines base fields before the derived constructor body runs.
- At `MakeClass`, build the full transition chain once (`{} → x → x,y → …`) and record the
  final shape on the class, analogous to `Chunk::obj_maps` for literals.
- `new C()` allocates with the final entry capacity: an inline slot class when the count is
  `≤ INLINE_PROPS`, otherwise one out-of-line `entries` block of the right size (no regrowth).
  The field stores then walk an already-built chain.
- Assignments to `this.x` in the constructor for JSDoc and `declare`-style fields use the same
  mechanism when the checker saw them on every path.

This stays a hint: if the constructor adds keys in another order (conditional fields,
`Object.defineProperty`), the object follows the ordinary transitions and loses nothing but
the reservation. The expected gain is mostly allocation count and memory. It is not a large
speedup, because shape transitions are already cached.

### 5.3 Typed shapes (the runtime foundation for stage 3, landed in stage 1)

For a class whose `ClassLayout.sound` holds (all fields typed and always initialized), its
instances get a **typed shape**: the ordinary `Shape` plus
- a `typed_class: u32` id and a *display* (`[u32; D]`, the class ids of its ancestors by
  depth), so "is an instance of C or a subclass" is one load plus one compare
  (`display[depth(C)] == C`); and
- a per-slot `TKind` vector for the fields.

Operations that can break the layout contract **de-type** the object. It moves to the
untyped twin shape (same keys, no class id) and a global **type epoch** is bumped. These
operations are all on slow paths or on paths whose ICs already key on shape:
- a store of a value whose tag does not fit the field's `TKind`. Stores from untyped code go
  through ICs, and an IC on a typed shape includes the tag test, which costs one compare;
- `delete`, `Object.defineProperty` on a field (accessor, non-writable), `setPrototypeOf`,
  and `__proto__` writes;
- `Object.freeze`/`seal`/`preventExtensions` do **not** de-type, because the layout is
  unchanged. Sound code never writes to a frozen object without the ordinary check (field
  stores in sound code keep a "writable" bit test, which is O(1) and folded with the shape id).

Prototype objects of sound classes are watched instead. Replacing or adding a method on
`C.prototype` (or on any ancestor, or on `Object.prototype`, which `proto_epoch` already
tracks) bumps `proto_epoch`.

Arrays: `number[]` needs no new representation. The f64 mirror with
`MIRROR_OK | MIRROR_NO_HOLES` *is* the typed layout. The one change is that an array which has
passed a `number[]` check gets a `typed` bit, and invalidating its mirror bumps the type
epoch. Arrays that never met sound code pay nothing.

Behaviour is always JS: a de-typed object is an ordinary object. Only sound code's
*assumptions* are withdrawn.

---

## 6. Stages 2–3: sound functions in native code

### 6.1 What the compiler may assume inside a sound function

- Locals and params of `Num`/`Bool` are unboxed F64/I32 with **no entry guards past the
  prologue and no kind checks** at their uses. Int32 is a *representation* choice, made by
  range analysis (loop counters bounded by `.length`, `|0`, `>>>0`, bit ops). Overflow
  branches to the f64 continuation in place; it is not a deopt.
- `Class(C)` values have C's typed shape (or a subclass's shape with C's display entry). Field
  `i` is at a fixed slot, loaded as `entries[i]`, and a `Num` field is read without a tag check.
- `NumArray` values have a live f64 mirror. Element `a[i]` is a bounds check plus
  `mirror[i]`. A store `a[i] = x` or `push(x)` in range writes both the mirror and the entry
  (the entry write can be deferred later; v1 keeps both).
- A call whose `SiteFact::Callee` names a sound function is a **direct call to its typed
  entry**: no identity guard. For a module-scope `function`/`const` binding the identity is
  immutable in ESM, or is watched by a watchpoint for `let`/CommonJS. Method calls
  `o.m()` with `o: C` are direct if no loaded subclass overrides `m` (class-hierarchy
  analysis over the checked program, plus a per-class "overridden or prototype-mutated"
  watchpoint). Otherwise they dispatch through the shape-cached method slot.
- Refcount traffic: parameters of `Class`/`Str`/`Array` kind are borrowed for the call and
  locals that do not escape are not cloned. This is the typed special case of J6
  (refcount elision).

### 6.2 Boundary checks (the only type checks in sound code)

A **check** is `(pc, TKind)` from `ChunkTypes`, compiled from a small program:

```rust
pub(crate) enum Check {
    Any,                                  // no check
    Tag(u8),                              // tag == t                (number, boolean, string, …)
    TagSet(u16),                          // (1 << tag) & mask != 0  (unions of primitives, T|null)
    Class { id: u32, depth: u8, nullable: bool }, // tag==OBJ && shape.display[depth]==id
    NumArray,                             // tag==OBJ && is_array && typed mirror OK|NO_HOLES
    Array,                                // tag==OBJ && is_array    (elements checked at use)
    Func { sig: u32 },                    // callable; typed entry used only if sig matches
    Object,                               // tag==OBJ (structural: nothing more)
}
```

Every check is **O(1)**: no traversal and no wrapper. Containers of non-number elements
(`string[]`, `C[]`) check the element at each read (transient-style), which a JIT makes nearly
free (ECOOP '19) and which LICM can hoist.

Where checks are placed:

| Boundary | Check | On failure |
|---|---|---|
| **Entry** from untyped code, via the generic entry, `call/apply`, a constructor call, or a callback from a builtin | `entry[i]` on each param and on `this` | `EXIT_ENTRY_FAIL`: the interpreter runs this call in bytecode (the path exists today) |
| **Casts** `as T`, `x!`, type-predicate narrowing (rows 2, 3, 5) | `Check(T)` at the expression | Exit to bytecode at that pc with the frame materialized; the bytecode continues with the value as-is |
| **Results of calls to non-sound code**, of untrusted `declare`d functions, and of implicit coercions | The declared result type | Same as casts: exit at the pc after the call, with the result on the stack |
| **Reads from untrusted places**: `Object`-typed properties, `T[]` elements, index signatures, `unknown` narrowed by `typeof` | The static type at use | Same |
| **Array bounds** without `noUncheckedIndexedAccess` | `i < len` | Same (JS yields `undefined`) |
| **Type epoch**, after any call into non-sound code or a coercion | `epoch == epoch_at_entry` (one load, one compare) | Same. The world changed; the rest of the call runs as JS |

The main design point: **a failed check is never an error.** The bytecode of the same
function computes the JS answer from the same state. This is also why checks can be
placed freely: their failure path already exists (the materializing exit of the current
JIT).

### 6.3 Calling convention: two entries per sound function

1. **Generic entry.** It is the existing `extern "C" fn(*mut JitFrame) -> u64` (and the
   `dentry` direct view). Its prologue runs the `entry` checks over `slots[0..n_params]` and
   `this`, unboxes into SSA, records `epoch_at_entry`, and jumps into the typed body. A boxed
   result is written as today. Untyped callers, the interpreter and builtins only ever see this
   entry.
2. **Typed entry.** Its signature is `extern "C" fn(frame: PTR, a0, a1, …) -> R`, where `aK`
   is F64, I32 or PTR by the parameter's `TKind`, and `R` is the unboxed return. A throw sets
   `frame.exception` and a status word the caller tests after the call (one load and branch,
   like helper `STATUS_THROW`). It runs no checks. Only sound code calls it, and only through
   a statically known callee or a `Func{sig}` check on a closure value whose chunk
   `ChunkTypes.sig` matches. `ChunkJit` gets a `tentry: Cell<usize>` beside `dentry`.
   `lumen-codegen` needs multi-argument signatures with mixed F64/I64 params and F64 returns
   (backend work, section 8).

Crossings:

| Caller → callee | Mechanism | Cost |
|---|---|---|
| untyped → sound | generic entry, with the checks | n tag/shape compares |
| sound → sound (known) | typed entry | a plain native call |
| sound → closure value `f: (x: number) => number` | if `f` is a sound closure with the same `sig`, typed entry; else generic call plus a result check | one compare, or a helper call plus a check |
| sound → untyped | `Helper::Call` with boxed args, then a result check and an epoch check | today's cost plus two compares |
| sound returns to untyped | box the result | a store |

No wrappers are ever created for function-typed values, which avoids the Typed Racket
failure mode. The price is that a sound function holding an untyped callback checks the
callback's result at every call. It never has to check the callback's behaviour.

### 6.4 Deopt and bailout

Sound code has **no speculative guards**, so it needs frame states only at:
- checks (6.2): cast, untyped-call result, untrusted read, bounds, epoch;
- helpers that can throw (`EXIT_THROW`), as today;
- safepoints (the budget counter), as today.

That is a strict subset of what the speculative tier already materializes. The existing exit
format (`(pc << 8) | kind`, write-back of SSA locals, the materialized operand stack) is used
unchanged. T2 (shared deopt stubs) applies to it and helps it. There is no lazy deopt of
*running* sound frames: watchpoints are observed at the epoch check after untyped calls
(frames above an untyped call cannot resume without passing one) and at loop safepoints.
A watchpoint that fires also marks the function's code `FS_PENDING`, so the next call
recompiles it against the new world.

### 6.5 Demotion

Every exit from a sound function is counted per pc. The existing `DEOPT_LIMIT` = 32 counts
resume exits. For sound code, entry failures and check exits beyond that limit **demote**
the chunk: its sound flag is cleared for this run, it recompiles through the stage-1 path
(ordinary speculation seeded by hints), and `--typed-report=runtime` records the pc and the
failing `TKind`. Programs whose types lie at run time therefore degrade to today's
performance, not below it.

### 6.6 Interaction with the existing tiers

- **The loop tier** (OSR) inside a sound function uses the sound compile too: at a loop header
  the slot kinds are known statically, so the OSR entry takes them unboxed after the same
  entry checks.
- **`inline.rs`** already infers kinds for small pure callees. For sound callees it can take
  `ChunkTypes` instead of inferring, and can inline more (property loads at fixed offsets
  are pure reads in sound code, with no exits).
- **wasm32** emits the same IR as a wasm module. Typed entries become wasm functions with
  `f64`/`i32` params, which wasm expresses directly.

---

## 7. AOT: typed native code in the blob (iOS, Puppeteer)

### 7.1 Blob changes

- **`include_js!` accepts `.ts`/`.mts`/`.cts`.** It runs the Rust stripper and checker at build
  time and honours `tsconfig.json` next to the entry (or `tsconfig = "…"`). Checker reasons
  can surface as `cargo` warnings under `typed_report = true`.
- **`SEC_TYPES`** (a new kind, one per unit) holds the `TypeTable` projected per *function
  index* (the key `SEC_BYTECODE` already uses) plus `ClassLayout`s. Only `TKind`s are stored,
  and it is small.
- **The bytecode codec must carry classes.** Today it refuses chunks with `MakeClass`, which
  removes exactly the class-heavy code the typed tier wants. This is a prerequisite milestone
  (A1) that is independent of types.
- **`SEC_NATIVE`** (one per unit *per target*) holds the machine code for each sound function
  (generic entry and typed entry) plus its exit tables (pc maps, frame-state write-back lists,
  and handler sets as in `Native::hsets`). `layout_fp` folds in the target triple, pointer
  width, `Value`/`JitFrame` offsets (`FRAME_*`), the helper table ABI and `TAG_*`. On a
  mismatch the loader ignores native sections and runs bytecode.

### 7.2 Code that is not in writable memory

iOS forbids making data executable, so the code cannot live in the blob's byte string.
Instead:
- The `include_js!` proc macro emits the native code as `core::arch::global_asm!` (`.text`
  section, `.byte` directives, one exported symbol per unit) for the target being compiled.
  The macro knows the target from `CARGO_CFG_TARGET_*`, and runs the codegen in-process
  because `lumen-codegen` is a Rust library. `SEC_NATIVE` then holds offsets into that symbol
  rather than code bytes.
- The code is **position-independent** and has **no absolute addresses**. Helper calls, shape
  ids, class ids, interned strings and constants go through a per-unit **link table** in
  writable data, which the loader fills (for example `link[k] = &helper_k` and
  `link[m] = typed_class_id(C)`). This is needed anyway, because shape ids are not stable
  across runs (the note on T5). Today's JIT uses `EXT_BASE + k` import ids, and those map
  directly onto link-table slots.
- The desktop AOT (Puppeteer) uses the same path. On JIT-capable hosts, a loader may still
  prefer JIT-compiled code if the AOT code is stale (a `layout_fp` mismatch).

### 7.3 What runs where on iOS

| Code | iOS today | iOS with typed AOT |
|---|---|---|
| Sound functions | interpreter | native (typed body; generic entry checks) |
| Everything else | interpreter | interpreter (unchanged) |

Speculative code can never be AOT-compiled usefully without profiles (T5), and even with
profiles its guards stay. Sound code needs neither profiles nor guards. **This is the
strongest reason for the whole plan:** it is the only route to native-speed JS on iOS that
does not change semantics.

---

## 8. Staged implementation plan

Each milestone is sized for one agent run, about 1–4 focused days, with its own tests.
"Gate" is the condition for merging. Every milestone requires test262 unchanged
(`LUMEN_JIT_EAGER=1` too) and the differential suite green.

**Front end**
- **F1: TS syntax parser** in `lumen-typescript`: statements, expressions, classes, and
  generics syntax, producing its own AST with spans. Gate: parses every `.ts` in a corpus
  (lumen tests plus a snapshot of the TypeScript repo's `tests/cases/conformance`) without
  panicking, and fails cleanly on the rest.
- **F2: Rust stripper.** Gate: byte-identical to `typescript_strip.js` over the corpus,
  including the same rejections and error codes. Then switch `module.js` to the Rust
  stripper.
- **F3: JSDoc extraction** of `@param`, `@returns`, `@type`, `@typedef`, `@template`, `@this`
  and casts, with tsc attachment rules. Gate: golden tests.
- **F4: tsconfig reader** (JSONC, `extends`, the options in §4.6).

**Stage 1 (hints)**
- **H1: `TypeTable` and `TKind` projection; the engine attach API; `Chunk::types`**, keyed by
  function start offset. Gate: `--typed-report` lists every function with its projected
  params.
- **H2: First-call compile with declared kinds** (§5.1). Gate: warm-up benchmark (a
  first-100-calls timing) improves, and a stress test shows that declaring a wrong kind
  changes nothing observable.
- **H3: Class layouts, pre-built transition chains, pre-sized `new`** (§5.2). Gate: the heap
  census (`LUMEN_HEAP_CENSUS`) shows no regrowth for typed classes.

**Stage 2 (checker)**
- **S1: Checker core**: primitives, locals, narrowing, function signatures, calls between
  checked functions, and the rules table §4.2 rows 1–9, 11, 16–18, 21, 24. Gate: one
  accept test and one reject test per row, and the reject tests each come with the
  adversarial program that would miscompile if the row were accepted.
- **S2: Classes in the checker**: rows 10, 12–14, 19; nominal subtyping; definite
  assignment. Gate: as S1.
- **S3: Casts, predicates, `declare`/`.d.ts` untrusted, intrinsics table** (rows 2–6, 20,
  §4.4), emitting `SiteFact::Check`s. Gate: as S1.
- **S4: Diagnostics**: `--typed-report` reasons, json output, and runtime counts.

**Runtime foundation**
- **R1: Typed shapes**: class ids, displays, per-slot `TKind`, and de-typing on every path in
  §5.3, plus the type epoch. Gate: an adversarial suite that mutates typed instances in every
  listed way and compares against `--typed=off`.
- **R2: Array typed bit and mirror-loss epoch bump.**
- **R3: Watchpoints**: per-class "overridden or prototype-mutated", module binding identity,
  and `Math` identity, replacing `MathGuard` re-checks. This overlaps T6; do it once.

**Stage 3 (native)**
- **N1: `lumen-codegen` signatures** with multiple mixed F64/I64/PTR params and F64 returns,
  on x86-64, AArch64 and wasm32. This is independent of types.
- **N2: Sound compile path in `build.rs`**: numeric-only sound functions (no objects), with
  the generic entry plus checks, the typed entry, and sound → sound direct calls. Gate:
  nbody-style kernels written in the subset run with zero entry failures, and the IR dump
  shows no guards.
- **N3: Checks and exits** for casts, untyped-call results, bounds and epoch (§6.2), with
  demotion (§6.5). Gate: the stress modes in §9.
- **N4: Classes in sound code**: fixed-offset field loads and stores, `new` with pre-shaped
  allocation, direct and virtual method calls.
- **N5: Arrays**: `number[]` via the mirror, `T[]` with element checks, and bounds-check
  elimination using the existing `i < a.length` fact and G5 when it lands.
- **N6: Int32 representation** by range analysis inside sound code, and refcount elision for
  borrowed params. This overlaps J1 and J6.

**AOT**
- **A1: Classes in the bytecode codec** (a prerequisite independent of types).
- **A2: `include_js!` for TS, and `SEC_TYPES`.**
- **A3: `SEC_NATIVE`** via `global_asm!` plus link tables; the loader, and the `layout_fp`
  fold. Gate: a typed benchmark runs natively in an AOT binary on desktop, and the same
  binary with `layout_fp` forced to mismatch runs the bytecode.
- **A4: iOS build** of an AOT sample (an interpreter-only build flag plus native sound
  functions). Gate: runs on a device or simulator and matches the desktop output.

Dependencies: F1 → F2/F3 → H1 → {H2, H3, S1}. S1 → S2 → S3. H3 → R1. {S*, R*, N1} → N2 → N3 →
N4 → N5 → N6. A1 is independent. {N3, A1, A2} → A3 → A4. Stage 1 (through H3) is
shippable on its own.

---

## 9. Verification

- **Differential by mode.** Every test program (lumen's own suites, a typed benchmark set,
  and a corpus of real TS projects' test suites where they run) runs under
  `--typed=off`, `hints` and `sound`, with and without `LUMEN_JIT_EAGER`, and with
  `LUMEN_NO_JIT`. Output and exit status must be identical. For AOT: the blob with and
  without `SEC_NATIVE`.
- **test262 unchanged** in every mode. It is plain JS, so any difference is a bug in shared
  machinery (typed shapes, epochs, watchpoints). Run it on the checkout per the Windows
  notes in the project memory.
- **Stress modes** (`LUMEN_TYPED_STRESS=`):
  - `fail-entry`: every generic-entry check fails, so every call to a sound function runs
    bytecode. The output must be identical.
  - `fail-checks`: every internal check (cast, result, bounds, epoch) fails at its first
    execution. This exercises every exit's frame state.
  - `detype`: every typed instance is de-typed at a safepoint and the epoch is bumped.
  - `generic-only`: sound → sound calls use the generic entry.
- **Boundary fuzzer.** A generator produces a sound module (classes, numeric functions,
  arrays, closures with function types) plus an untyped *adversary* that:
  - passes wrong-typed arguments, `this`, and subclass instances;
  - mutates typed instances in every §5.3 way;
  - pushes strings into `number[]`s held by sound code;
  - replaces prototype methods and `Math` functions mid-run;
  - throws from callbacks.
  The oracle is `--typed=off`. It extends the existing JIT differential fuzzer.
- **Checker soundness tests.** For each row of §4.2, a program that tsc accepts, that lumen
  must mark not sound (or compile with a check), and that produces wrong output if the rule is
  disabled (a `cfg(test)` switch per rule). This proves each rule is load-bearing.
- **Stripper equivalence** (F2): byte-identical to the JS stripper over the corpus.
- **Performance tracking.** For the typed benchmark set, compare typed against the same code
  stripped, on JIT, AOT and the interpreter. Track `--typed-report` sound percentages on real
  codebases (lumen's own JS/TS builtins where applicable, Puppeteer's TS sources).

---

## 10. Expected speedups (honest)

Baselines: lumen's JIT today (about 10x behind V8 on V8-v7), and lumen's interpreter (iOS).

| Code category | Stage 1 (hints) | Stage 3 vs lumen JIT | Stage 3 AOT vs interpreter (iOS) |
|---|---|---|---|
| Numeric kernels (nbody, spectral-norm, matrix, FFT, hashing with bit ops) | Warm-up only: native from the first call instead of after 1024 calls. About 0% steady state | 1.5–3x (guards, boxing and tag checks go; int32 needs N6) | 10–30x. Static Hermes saw about 10x typed vs untyped on nbody, and ~20–40x once inlining landed |
| Class-heavy OO (DeltaBlue, Richards, ray tracers) | Allocation and memory (pre-sized objects): 0–10% | 1.5–3x, **if** calls are direct and inlined (J2/J3). Fixed offsets alone save little over monomorphic ICs; STS's data says layout matters more than checks | 5–15x |
| Mixed apps (Puppeteer drivers, CLIs, servers) | Warm-up: noticeable on short runs | 0–20%: most time is in untyped libraries, builtins, IO | 1.2–2x, limited by the share of sound code |
| String, regex, JSON, IO-bound | ~0% | ~0–5%: time is in the runtime (string ops, regex engine, GC, syscalls) | ~0–10% |

Caveats:
- These numbers are **estimates** from the research in §2, not measurements.
- A mature speculative JIT already recovers most of the stage-3 gains on monomorphic code, as
  the TC39 proposal's authors note. The gap closes as lumen's own speculative tier improves
  (inlining, int32, load CSE). **Stage 3's lasting value is AOT/iOS and predictability**, not
  beating V8.
- Fine-grained typed/untyped mixing (a sound helper called from a hot untyped loop) pays
  entry checks per call: a few compares, bounded, never Typed Racket's 100x. Still, it can be
  slower than well-speculated untyped code if the checks cannot be hoisted. Demotion (§6.5)
  bounds the damage from lying types, but not the cost of honest checks.
- The yield depends on how much code is sound. Idiomatic TS uses `any`, structural
  interfaces, `as` casts and async functions heavily. Expect small sound percentages in
  application code and high ones in numeric and data-structure libraries written for it.
  `--typed-report` exists to make this visible and actionable.
- The largest risk is correctness: a checker bug is a miscompile. §9 is sized accordingly,
  and every trusted fact has a stress mode that withdraws it.

## 11. Open questions

- **Strict mode.** Should an opt-in Static-Hermes-style `--typed=strict` (a failed check
  throws `TypeError`) exist, trading semantics for fewer exits and frame states? Not before
  N3 has data on how often exits happen.
- **Specializing generics** (`Vec<T>` for `T = number`) needs monomorphization in the checker
  and code duplication in AOT. Deferred.
- **Integer types.** TS has none. Should the checker honour a branded `type i32 = number &
  {__i32: never}` convention, or rely on range analysis only? The proposal is range analysis
  only, because it does not change the TS the user writes.
- **Structural types.** Interfaces implemented by exactly one class in the checked program
  could be treated nominally with a runtime check; this is StrongScript's `!C` inferred.
  Measure first.
