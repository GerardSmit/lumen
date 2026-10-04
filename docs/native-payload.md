# JS native payload version 3

Encoder: `crates/lumen/src/precompiled/native_host.rs` and
`crates/lumen-aot/src/native.rs`. All integer fields are little-endian `u32`
unless specified. A string is a byte length followed by engine UTF-8 bytes.
Counts precede every repeated list. No field contains an AST, bytecode or a host
address. Nonliteral constants and uncompiled expressions are rejected by the producer.

Header: `LUMJSN03` (8 bytes), version `3`, function count, unit count,
entry unit (`u32::MAX` for none).

Version 3 retains the version 2 unit/function record order below. After the last
function record, `snapshot_len` is followed by that many portable `LMSNAP` v1
bytes, followed by `snapshot_entry_function` (`u32::MAX` for none). Zero length
means no initialized heap snapshot. Snapshot node references and
native function indices are integers; host addresses and source replay are forbidden.

Unit records: path string, kind (`0` script, `1` module, `2` CommonJS),
top-level function index, link list (`specifier string`, target unit index),
declaration list, import list, export list, then a string list of literal
CommonJS `require` dependencies. `links` maps each dependency to its bundled
target. Requires execute lazily at the call site; their presence is not an
instruction to run every dependency during ES module initialization.

Declarations: kind `u8` (`var 0`, `let 1`, `const 2`, function `3`, class `4`,
using `5`, await using `6`), name string, slot index or `u32::MAX`, function
index or `u32::MAX`.
Imports: source string, spec list (tag `u8`, imported string, local string).
Spec tags: default `0`, namespace `1`, deferred namespace `2`, source `3`, named `4`.
Exports: tag `u8`, source string, local string, exported string.
Tags: local `0`, reexport `1`, star `2`, namespace `3`.
Default exports use an internal `%lumen-default%` binding. Module-only syntax
is removed from the host entry body. Import attributes remain unsupported.

Function records, in order:

1. Name string, flags, declared length, slot count, parameter slot count, max stack.
2. Arguments slot, rest slot, virtual base (`u32::MAX` for absent), frame flags.
3. Property cache count, name cache count, object template count.
4. Forced reset slot list.
5. Capture initializer list: tag `u8`, then fields:
   `0` parameter: slot, name; `1` var: name;
   `2` function: child-local index, name; `3` lexical: immutable `u8`, name.
6. Child function index list (global image indices).
7. Name string list, slot-name string list, constant list.
8. Resume list (`pc`, stack depth), position list (`pc`, source offset).
9. Class list.

Class record: name string, superclass evaluation function or `u32::MAX`,
decorator evaluation function list, ordered member list. A superclass means
derived construction; an absent constructor means the runtime supplies the
appropriate default constructor.

Member record: kind `u8` (constructor `0`, method `1`, get `2`, set `3`,
field `4`, accessor `5`, static block `6`), static `u8`, key tag `u8` and data,
method function or `u32::MAX`, initializer function or `u32::MAX`,
anonymous initializer `u8`, decorator evaluation function list.
Key tags: public string `0`, private identifier string `1`, number bits `2`,
computed-key evaluation function `3`. Private names preserve their leading `#`.
Member order preserves static initialization order; key identity pairs private
accessors. Static methods use the constructor as home; instance methods use its
prototype. The runtime must establish the class private environment and
`%fieldinit%` context for field initializers.

Function flag bits: arrow `1`, strict `2`, generator `4`, async `8`,
method `16`, named expression `32`, top level `64`.
Frame flag bits: uses this `1`, environment this `2`, derived `4`, reflect arguments `8`.

Constant tags (`u8`): undefined `0`; null `2`; false `3`; true `4`;
number `5` followed by 8-byte IEEE bits; bigint `6` followed by decimal string;
string `7` followed by string. Tag `1` is reserved. Empty, symbol and object
constants are producer errors.

Each unit's entry precedes its functions in snapshot traversal order, followed
by synthetic class expression functions. Native
helper GOT indices are the frontend import ids, including entry/safepoint/
landing/resume ids `0xff0` through `0xff3`. `compiler_ready()` gates CLI
publishing while top-level binding and native runtime semantics are incomplete.
