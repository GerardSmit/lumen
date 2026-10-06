# Native WebAssembly JS API

`WebAssembly` (`Module`, `Instance`, `Memory`, `Table`, `Global`, `CompileError`, `LinkError`,
`RuntimeError`, `validate`, `compile`, `instantiate`) is one `lumen_bind` module,
`wasm_ops::api` in `lumen-web`. `js/wasm.js` and the raw `__wasm` op namespace are gone; the
decoder, validator and interpreter (`lumen-web/src/wasm/`) are unchanged.

## Publication

`wasm_ops::install` publishes `WebAssembly` through `Ctx::install_lazy_global_group`: the global
is an accessor until first touched, then the namespace object is built with
`install_module::<api::Module>` and replaces it (writable, non-enumerable, configurable). A script
that replaced the global first keeps its value. The Bitnest kernel does not ship the web glue
(no WebAssembly there), so it needs no change.

## Design

- **Store addresses.** All entities live in one `Store` in `WasmStore` (OpState). `Memory`,
  `Table` and `Global` wrappers hold an address; `WasmStore::objects` maps `(kind, address)` to a
  weak wrapper, so a store entity has one JS identity while any wrapper is alive (a re-exported
  imported memory is the same `Memory`, `table.get(i)` is the same function an instance exports).
- **Functions.** Exported functions are native closures. The address lives in a hidden native
  slot (`FUNC_SLOT`), which `Table.prototype.set` reads to accept only wasm functions.
- **Instances.** `instance.exports` is a prototype getter returning a frozen null-prototype object
  held in a hidden native slot of the wrapper (a traced GC edge, not script-visible).
  `Ctx::freeze_native_object` is the engine-private `Object.freeze`.
- **Errors.** The three error classes are `hint(js(error))` native classes. Their constructors
  return `ErrorInit`, a `CtorRet` that defines an own, non-enumerable `message`; the prototypes get
  `name` and an empty `message` from the module `#[init]`. Native code throws them with
  `error_value`.
- **Promises.** `compile` / `instantiate` take `Value` arguments and return `Promise::ready`, so
  every argument error is a rejection.
- **Streaming.** `compileStreaming` / `instantiateStreaming` did not exist in `wasm.js` (only
  `lumen-runtime`'s `process.js` deletes them under `--no-experimental-fetch`); they are still
  absent.

## Behavior changes

- Wrong argument kinds throw `TypeError` (`validate("x")`, `new Module(5)`, `new Instance({})`,
  a missing or non-object descriptor, an unknown `Global` type, a non-wasm value in `Table.set`);
  `validate` no longer swallows them.
- Imports follow the JS API: missing import object or module namespace is a `TypeError`; a wrong
  import kind, a non-callable function, or a mutable global that is not a `WebAssembly.Global` is a
  `LinkError`; a number or BigInt satisfies an immutable global of the declared type (it was typed
  from the value before).
- `exports` is frozen with a null prototype and an accessor on `Instance.prototype` (it was an
  own, writable data property), and exported mutable globals are mutable (they were always
  immutable).
- Errors are real classes with the spec shape: own `message`, prototype `name`, instances of
  `Error`; before, `name` was an own enumerable property.
- `Table` accepts only `element: "anyfunc"` / `"funcref"` when given.
- Methods and accessors of the interfaces are enumerable (Web IDL), and calling a constructor
  without `new` throws.

## Tests

`crates/lumen-runtime/tests/browser_apis.rs` (`webassembly_*`) covers shape, exports, imports,
errors, entity identity, promises and GC; `crates/lumen-web/tests/wasm_shared_memory.rs` covers
shared memory.
