# RE:Dox and structured data in Lumen: what to take, what to leave

Research only; nothing was built or benchmarked. REDox paths are relative to `~/code/research/REDox/src/`
(`REDox/` = core, plus `REDox.Xml`, `REDox.Csv`, ...). Lumen paths are relative to `/Users/gerard/code/lumen/crates/`.
Claims about third-party crates (simd-json, quick-xml, ...) come from memory of their APIs, not from reading
their source in this pass; they are marked "(check)" where a design decision hangs on them.

## 0. Verdict in six lines

1. Lumen already has the shape RE:Dox's *write* side and its "format-agnostic converter" idea aim at: one
   language-neutral parser in `lumen-common` that drives a per-language `Sink` straight into native values
   (`lumen-common/src/json.rs:328-362`). JS `JSON.parse` and Python `_json` both ride on it today.
2. A token tape does not beat that for the hot paths (`JSON.parse`, `json.loads`, `csv.reader`, `pyexpat`):
   both engines must materialise full GC/Rc values anyway, so a tape adds a pass and an 8-byte-per-token
   intermediate for nothing. RE:Dox's reported wins are *.NET typed deserialisation* wins (no DOM allocation
   vs System.Text.Json's DOM + reflection), and its parallel trick needs `Send` values, which Lumen's engine
   values are not.
3. A tape earns its keep only where Lumen wants lazy access, in-place editing that preserves comments and
   layout, or cross-format conversion. Lumen has none of those features today; the config files it reads
   (`tsconfig.json`, `package.json`, `pyproject.toml`) are tiny and read-only.
4. Do not build a general `lumen_common::doc` IR now. Do (a) harvest RE:Dox's *scanner and writer* ideas
   into the existing `json.rs` (measurable, small), and (b) if and when an editing feature appears, wrap
   `toml_edit` (TOML/pyproject) and write one small trivia-preserving JSON/JSONC tape, not ten front-ends.
5. Keep the SAX/handler design for XML (`pyexpat` is expat-parity streaming) and the spec state machine for HTML.
6. First step: a criterion-style bench of `lumen_common::json::Parser` + `quote_into` on canada/citm/twitter
   (the same three files RE:Dox publishes), then a memchr/SWAR string scan. Details in section 5.

---

## 1. How RE:Dox works

### 1.1 Token layout (the heart of it)

One token is one `long` (64 bits): `DToken` is a struct with a single `_value` (`REDox/DToken.cs:307`, constants
at `:13-17`). There is no separate "type, length, offset" struct; everything is bit-packed. Two modes selected by
the sign bit (`DToken.cs:159` `IsExtended => _value < 0`):

- **Source-backed (bit 63 = 0)**: the payload points into the original input. Layout from
  `REDox/DToken.Internal.cs:234-246` and `DTokenKind.cs`:
  - bits 62-60 = `DTokenType` (Ignore 000, Literal 001, Number 010, ExtNumber 011, Text 100, Binary 101,
    Map 110, Array 111; `DTokenType.cs`), bits 59-56 refine to `DTokenKind` (Control, Trivia, Boolean, Null,
    Integer, Float, BigNumber, InlineFloat, String, Symbol, ByteString, Timestamp; `DTokenKind.cs`), bits 58-56
    carry a per-kind sub-kind (`DTokenVariant.cs`: e.g. `StringDoubleQuote`, `IntegerHexadecimal`,
    `TriviaLineComment`, `TimestampLocalDate`).
  - Scalars: `payload = offset (31 bits) | length (25 bits) << 31`
    (`EncodeLengthOffsetPayload`, `DToken.Internal.cs:234`). So a 2 GiB source and 32 MiB values max.
    Longer strings are encoded as `length >> 8` under a different variant (`StringMultilineDoubleQuote`,
    `JsonDocument.cs:696-705`) and the true end is recovered by scanning for the quote at decode time
    (`GetStringBlob`, `JsonDocument.cs:1378-1423`).
  - String "escaped?" flag lives in the length bits for text formats (`EncodeStringParamPayload`,
    `DToken.Internal.cs:211`) or in the variant (`String` vs `StringDoubleQuote` = "has a backslash"), so
    unescaped strings decode as a straight slice.
  - Containers (`MakeArray`/`MakeMap`, `DToken.cs:31-45`): `count` in bits 0-29, `link` in bits 30-59.
    `link` is the **tape index of the next sibling after this container's subtree** (`LinkToken`,
    `Document.cs:230`; `NextToken`, `:247`: a container's next is `LinkId`, anything else is `id+1`).
- **Extended (bit 63 = 1)**: payload meaning is fixed by RE:Dox, used after mutation: inline 53-bit integers
  (`MakeExtendInlineInteger`, `DToken.Internal.cs:50`; range `MinInlineInteger..MaxInlineInteger = +-2^52`), inline
  `float32`, a 59-bit inline double covering exponents 992..1055 (`EncodeInlineFloatPayload`, `:251`), literals
  with a trivia id, or a 28-bit **index into a side table `_extends`** holding the real object (managed
  string, byte[], container object, trivia list). Free-list control tokens (`MakeEmpty`, `MakeEmptyExtend`)
  chain recycled slots.

Tape order is document order, depth-first. A map is `[Map count link] key value key value ...` where a
key's value token follows it and a container value's `link` jumps to the next key. An array is
`[Array count link] v v v ...`. `ValueEnumerator`/`KeyValueEnumerator` (`Document.Enumerator.cs:92-...`) walk
`count` children, advance by `token.IsContainer ? LinkId : id+1`, and skip `IsIgnore` tokens (trivia and
jump control tokens, `DToken.cs:173`).

Observations that matter for a port:
- Skipping a subtree is O(1) only forward and only where `link` was set. At parse time the link is written when
  the *next sibling* is allocated (`JsonDocument.cs:709-715`, `LinkToken(latestId, tokenId)`), so the last child
  of a container has `link = 0`. Enumerators never need it because they stop at `count`. simdjson's tape stores
  the end index on the opening token, which does not have this hole.
- Subtree size is `link - id`, which the parallel deserialiser uses as a free cost estimate (1.5 below).
- Object member lookup on an unmodified tape is a linear scan (`DElement.TryGetProperty`, `DElement.cs:644`).
  A hash index is built only when a container is first mutated (`DObject` has `_buckets/_hashCodes/_next`,
  `DObject.cs:14-22`).
- The parent of a token is not stored; `GetParentId` builds a whole-document parent table lazily
  (`Document.Internal.cs:172-238`) and throws it away on every edit (`_parentVersion = 0`, `:712`).

### 1.2 Strings and numbers

- Parse does **not** decode. Strings are `(offset, length, escaped?)`; numbers are `(offset, length, kind)` where
  the parser only classifies `Integer` / `IntegerUnsigned` / `Float` / big (`JsonDocument.cs:960-1020`; >19 digits
  goes through `Utf8Helper.GetIntegerTokenKind`, `Utf8Helper.cs:1603`; `-0` is forced to `Float`, `:1019`).
- Decode is on request via the .NET `Utf8Parser` (`DecodeFloat`, `JsonDocument.cs:1505`; `DecodeInteger`, `:1333`),
  and strings go UTF-8 -> UTF-16 `string` each time asked (`DecodeString`, `:1425`): there is no interning or
  string table, so repeated reads allocate repeatedly. `Utf8Symbol` (`Serialization/Utf8Symbol.cs`) is a
  pre-encoded property-name cache used on the *write* side only.
- Number text can be re-emitted unchanged (`WriteNumberString`, `DataWriter.cs:265`), which is how round-trips
  keep `1.0`, `1e5`, hex and so on exactly.

### 1.3 Mutation on a tape

Reads are tape; writes are copy-on-write per container.
1. `GetElementObject/Map/Array` (`Document.Internal.cs:253-321, 503-600`) materialise a managed `DObject`/`DMap`/
   `DArray` holding `KeyValuePair<uint,uint>[]` of *token ids* (not values), register it in `_extends[]`, and
   overwrite the container's tape slot with an extended container token pointing at it. From then on the
   container's children are enumerated from that array, not from the tape.
2. Inserting a scalar allocates a fresh token at the **end of the tape** (`AllocToken`, `Document.cs:211`) or
   reuses a free-list slot (`AllocEmptyToken`, `Document.Internal.cs:62`) and appends its id to the container.
   Strings/blobs go to `_extends` (`ExtendToken`, `:1381`).
3. Removing frees the token ids onto the free lists (`Free`, `:664`); the old subtree's tape region is left as
   garbage and, where it would break the forward `link` chain, a `Jump` control token is written into the slot
   after the replaced token (`MakeJump`, `Document.Internal.cs:1272`, `:1401`).
4. `Version++` invalidates outstanding handles (`DContainer.IsValid`, `DContainer.cs`; `DTrivia.IsValid`).
5. `Duplicate()`/`CreateSnapshot` deep-copies the tape and clones every live container object
   (`Document.cs:89-141`).

So it is a "tape with indirection", not a real tape any more once edited: the edit cost is paid per container
touched, and the tape itself only grows. There is no compaction in what I read.

### 1.4 Trivia (JSON5/TOML/INI/XML comments)

- Trivia are ordinary tokens (`TriviaWhitespace`, `TriviaLineComment`, `TriviaBlockComment`, `TriviaSeparator`
  for `,` and `:`, `TriviaTag` for CBOR tags, `TriviaStyle`; `DTokenKind.cs`/`DTokenVariant.cs`). With
  `PreserveTrivia` the JSON5 parser emits them *before* the token they lead (`Json5Document.cs:572-585` whitespace,
  `:699-796` separators, `:1062-1116` comments). They have `Type == Ignore`, so value enumerators skip them
  for free, and `TriviaTokenEnumerator` finds a token's leading trivia by walking *backwards* from its id while
  `IsLeadingTrivia` (`Document.Enumerator.cs:23-60`).
- After a token is edited (extended), its trivia list moves to a `List<uint>` in `_extends` and the token carries a
  `TriviaId` (`DToken.Internal.cs:125-139`; `GetElementTriviaList`, `Document.Internal.cs:323`).
- The writer re-emits trivia around values, tracking separator state to keep trailing commas and indentation
  consistent (`Json5Document.cs:1455-1500, 1625-1810`, `SeparatorState` `:2342`). Layout inside an edited container
  is therefore "original trivia + normalised separators", not byte-exact.

### 1.5 Format front-ends onto the model

All are single-pass hand-written scanners that `AllocToken` straight into the tape. Only JSON (`JsonDocument.cs`
1824 lines), JSON5 (2575), CBOR (1403), MessagePack (1218) are "integrated"; TOML, XML, HTML, CSV are labelled
preview in `README.md`.

| Format | Mapping (file) | Notes |
|---|---|---|
| JSON | map/array/scalars as above (`JsonDocument.cs:616`) | NDJSON via `ParseNDJson` (`:562`); `EnsureCapacity(len/16)` presizes the tape (`:537`) |
| JSON5 | same + trivia tokens (`Json5Document.cs:522`) | single quotes, hex ints, identifiers kept via `StringKind`/`IntegerKind` sub-kinds |
| CBOR / MsgPack | `offset` points into the binary source; document layer decodes (`CborDocument.cs:529`) | tags kept as `TriviaTag` only if `PreserveTag` and unknown (`:930`); indefinite-length strings, timestamps, big numbers have their own kinds |
| XML | `XmlDocument.cs:255` | root = Array of children. Element = **Map** whose entries are `attr-name, attr-value, ...` then **one final entry `tagName -> Array(children)`**; mixed content is the child Array holding Strings (text) and Maps (elements); comments are `TriviaBlockComment`, PIs have opaque text (`:571`). **No namespace resolution**: `a:b` and `xmlns:a` are just strings; no DTD/entity expansion beyond the "has & and ;" escape flag; whitespace-only text is dropped (`XmlDocument.cs:292`: only text with a non-space byte is emitted). It is a structural parser, not an XML Infoset. |
| HTML | `HtmlDocument.cs` | same XML-like shape; void tags (`IsVoidTag`, `:1018`), raw-text `script`/`style` (`:659`), a fixed named-entity table. "Practical", not the WHATWG tree builder. |
| CSV | `CsvDocument.cs:272` | Array of rows; with `HasHeaderRecord` each row is a **Map** and the header token (8 bytes, source-backed) is *copied into every row* (`:315-335`); otherwise rows are Arrays. Strict column count. |
| INI | `IniDocument.cs:439` | root Map; `[section]` becomes key -> Map; global keys sit in the root; duplicate-key checks via `HashSet<string>` (`:448`); section/`=`/whitespace trivia preserved on request |
| TOML | `TomlDocument.cs:556` | tables are Maps, arrays of tables are Arrays of Maps. `[a.b]` headers that revisit a table go through `SelectTable` (`:364-536`), which **linearly scans children** and splices new tokens via `LinkToken` plus `Nop`/jump slots. Dates map to `Timestamp*` kinds. Comments are trivia. `SetCollapsed` hashes keys for layout hints (`:56`). |
| YAML | `experimental/REDox.Yaml` | not integrated |

### 1.6 Reader / writer abstraction

- `DataWriter` (`Serialization/DataWriter.cs`) is a push, event-style writer: `WriteStartMap(int? definiteLength)`,
  `WriteString(ReadOnlySpan<byte>)`, `WriteNumberString`, `WriteBigNumber`, ... plus *fused* helpers
  `WritePropertyInt32(Utf8Symbol, int)`, bulk `WriteDoubleValues(ReadOnlySpan<double>)` (`:75-210`). It is the
  serde-`Serializer` of RE:Dox; each format subclasses it (`JsonWriter.cs`, `CborWriter.cs`,
  `MessagePackWriter.cs`, `Utf8TextWriter.cs` shared by text formats, `Document.TokenWriter.cs` writes *into a tape*).
- `DataReader` (`Serialization/DataReader.cs:11`) is a `readonly ref struct` over `(Document, root id)` with
  `ReadInt32(tokenId)`, `EnumerateMap(tokenId)`, `GetToken(tokenId)`: **random access by token id**, not a
  cursor. Converters (`DataConverter<T>`) take `(reader, tokenId)`.
- Format conversion is `Document.WriteToken(writer, reader, tokenId)` (`Document.Internal.cs:2957-3062`): a recursive
  tape walk that calls the matching `Write*` per `DTokenKind`. That single function is the whole "JSON -> CBOR"
  story; kinds that the target cannot express are lost or downgraded (no loss report).
- Asymmetry rationale (`README.md` "Asymmetric read/write design"): writes know their values, so no DOM; reads need
  look-ahead for `$type`, `$id/$ref`, out-of-order constructor args, so build the index first.

### 1.7 The parallel deserialisation trick

`ParallelSzArrayConverter<T>` (`Serialization/Converters/ArrayConverter.cs:573-650`):
1. Cost estimate from the tape for free: `tokens = token.LinkId - tokenId`, or `count * (firstChild.LinkId - firstChild)`
   when the array's link is unset (`:608-627`). Thresholds `MinimumNumberOfElements = 4`,
   `MinimumNumberOfTokens = 1000`, `MaxDegreeOfParallelism = 4` (`ParallelDeserializeOptions.cs:10-16`).
2. One serial walk collects the child token ids into a pooled `uint[]`.
3. `Parallel.For` over the ids; each worker builds its own `DataReader(rootElement)` (a ref struct over the shared
   immutable tape) and decodes its subtree to a managed object.

It works because the tape is immutable during the read, subtrees are disjoint index ranges, and .NET objects
are shareable across threads. Parse itself stays single-threaded, and it is disabled with `$id/$ref`
(`ArrayConverter.cs:109`). Published numbers: 2.25-2.84x over STJ for parallel vs 1.36-1.77x sequential
(`README.md` table; AMD Threadripper PRO 5975WX, .NET 10; their own caveat that it is not universal).

### 1.8 Performance techniques

What is actually in the source, as opposed to the README prose:
- **No hand-written SIMD.** A grep for `Vector128/256`, `Avx2`, `Sse2` over `src/` finds nothing. Vectorisation
  comes from BCL calls: `IndexOfAny('"','\\')` in string scanning (`JsonDocument.cs:670`) and `SearchValues<char>`
  for escape detection in the writer (`Json/JsonTextEncoder.cs:17,93`). The parser is a scalar `goto`-driven state
  machine over `ReadOnlySpan<byte>`.
- **Lazy decode**: strings/numbers/timestamps stay as slices; an unused field costs 8 bytes of tape.
- **Tape presizing**: `EnsureCapacity(source.Length / 16)` (`JsonDocument.cs:537`) and 8 B/token.
- **Pooling**: `ArrayPool<DToken>/<byte>/<object>/<uint>` for tape, input copy, `_extends`, parent table
  (`Document.Internal.cs:31-125`); `Helper.InstanceCache<T>` is a 32-slot static pool of writers cleared on gen2 GC
  (`Helper.cs:408`); parser stacks are `stackalloc` (`LocalStack`, `JsonDocument.cs:618`).
- **Typed-array fast paths**: `ReadTypedArray` / `WriteDoubleValues` bulk spans for primitive arrays.
- **Output-side**: fused `WriteProperty*` with pre-encoded `Utf8Symbol` names, direct `IBufferWriter<byte>`.

Takeaway: the speed is "do not decode, do not allocate a DOM, do not reflect", i.e. wins against .NET's DOM +
reflection stack. Lumen's baseline is already a no-DOM streaming parser, so the same ratio will not repeat.

### 1.9 Weak spots to avoid copying

- Tape ids are `uint` into a shared mutable array; every handle carries `(Document, id, version)` and a version
  check. In Rust this becomes an arena with generation or a borrow-checked immutable tape; the mutable
  layer needs its own design.
- Parent lookup is O(n) rebuild per edit epoch; TOML table lookup is O(children) per header (quadratic for
  large `[[array]]` heavy files); object lookup is linear until first mutation.
- Source-backed tokens pin the whole input buffer for the document's lifetime; `Duplicate()` copies it.
- 31-bit offset, 25-bit length, 30-bit link/count caps (2 GiB / 32 MiB / 1 G entries) are asserted, not recoverable.
- XML has no namespace handling and drops whitespace-only text, so it cannot be an engine DOM/`pyexpat` substrate.
- Number text round-trip relies on the *source* staying alive; once edited, formatting falls back to `double`.

---

## 2. Inventory of Lumen's current implementations

Sizes are line counts. "Adapter" = who calls the shared core.

| Format | Core (file, lines) | Adapters / users | Notes |
|---|---|---|---|
| **JSON parse** | `lumen-common/src/json.rs` (1018): `Parser` recursive descent (`:365-740`), `Sink` trait (`:328`), `Number` lazy small-int (`:285`), `Str` Cow borrowing source (`:273`), `Value` tree + `parse`/`parse_jsonc` (`:742-790`) | JS: `lumen/src/builtins/json.rs:98-127` (`JsSink` `:610`, `RecordSink` for reviver `context.source` `:691`); Python: `lumen-py/src/builtins/jsonm.rs` (872; `scan_once` `:281`, `PySink` `:314`, `scanstring` `:130`); tools: `lumen-runtime/src/tsconfig.rs:99-103` (`parse_jsonc`), `lumen-runtime/src/esm.rs:826,896` (package.json), `lumen-aot/src/walk.rs:250`, tests (html5lib, jsx conformance), fuzz `fuzz/fuzz_targets/json_parse.rs` | Already one implementation, UTF-16 / code-point `Spelling` for lone surrogates (`lumen-common/src/smuggle.rs`), CPython-compatible error kinds and byte positions. JS keys dedup through a per-parse `FastMap<&str, Rc<str>>` (`lumen/src/builtins/json.rs:613`). Scanning is byte-at-a-time: `string_body` `:634-655`, `ws` `:393`. |
| **JSON write** | `json::quote_into/escape_into` (`lumen-common/src/json.rs:72-130`), byte loop with branches | JS `JSON.stringify` walker in `lumen/src/builtins/json.rs:158-415` (`JsonSer`, `property` `:234`) (calls `quote_into` `:302,:390`); Python native `make_encoder` in `jsonm.rs:688`; `console_fmt`, `lumen-runtime/src/lib.rs:2151`, JSX codegen | Walks live engine objects (replacer/toJSON/indent semantics are language semantics); only quoting is shared. |
| **JSONC** | same parser, `Options::JSONC` (`json.rs:266`) | `tsconfig.rs`, package manifests | Comments, trailing commas, BOM; no trivia kept. |
| **JSON5** | none | none | Not needed by any current consumer. |
| **CSV** | `lumen-common/src/csv.rs` (463): CPython `_csv` state machine, `Dialect` (`:41`), `Parser::feed_line` (`:155`), `RowWriter` (`:334`) | `lumen-py/src/builtins/csvm.rs:73` (664 lines adapter); `csv.py` vendored (`lib/MODULES.txt:98`) | Line-oriented to match PEP 305 semantics (quoting modes, `skipinitialspace`, `strict`). No JS CSV. |
| **XML** | `lumen-common/src/xml/` (mod 457, parser 1874, scan 432, dtd 461, chars 189): expat-parity streaming, namespace-aware, `Handler` trait (`mod.rs:371`) | `lumen-py/src/builtins/pyexpatm.rs:13,304` (1162) -> `xml.etree`, `xml.dom.minidom`, `xml.sax`, `plistlib` (all vendored, `MODULES.txt:229-253`) | Only user of `lumen_common::xml` today. |
| **XML (browser)** | `lumen-html/src/xml.rs` (840): its own bounded parser straight into `lumen_html::Document` arena (`lib.rs:170`), 4 MiB cap, depth 512, resolves element namespaces, `no_std` | JS `DOMParser`/XML documents: `lumen-html-js/src/document_utilities.rs:45`, `browsing_context.rs:443,448` | Being migrated by another agent onto `lumen_common::xml`; there is a conflict between "expat-parity with error codes" and "bounded, no_std". |
| **HTML** | `lumen-html/src/html.rs` (5071): spec tokenizer + tree builder, entities (`entities.rs` 2236), `name.rs` interned atoms (`:8`), `Tag` enum (`html.rs:625`) | `lumen-html-js`, `lumen-html-text`, `lumen-html-image`, fuzz `html_parse.rs`, html5lib conformance tests | Builds `Document` node arena with a mutation journal (`lib.rs:170-185`). |
| **INI** | none native. Python `configparser` is **not vendored** yet (absent from `lib/MODULES.txt`; pure Python in CPython) | nothing | CPython has no C accelerator, so no native module is required. |
| **TOML** | none in the engines. `tomllib` not vendored yet (pure Python + `re` in CPython). | `lumen-cli/Cargo.toml:27` `toml = "0.8"` -> `lumen-cli/src/aot_config.rs:93-98,165` (pyproject / lumen.toml, converted to `serde_json::Value`) | Only Rust consumer, config load. |
| **plist** | `lib/plistlib.py` (pure Python; XML via pyexpat, binary via `struct`) | Python only | No Rust plist crate in the workspace. |
| **CBOR / MessagePack** | none | none | `ciborium`, `rmp` exist in the cargo registry cache but are not dependencies. |
| **Pickle / marshal** | `lumen-common/src/pickle.rs` (545) + `lumen-py/src/builtins/picklem/*` (3400) + `marshalm.rs` (710) | Python only | Binary, Python-object-graph formats; out of scope for a document IR. |
| **serde_json** | `lumen-cli/Cargo.toml:26`, `lumen-html-image/Cargo.toml:21` | `aot_config.rs`, manifests, html-image | Separate from `lumen_common::json::Value`; two JSON trees coexist in tooling (only `lumen-common` is in the engine). |

Not found anywhere: YAML, JSON5, CBOR, MessagePack, INI, a JSON `Number` that keeps big integers (the
`Value::Num(f64)` tree loses ints > 2^53, harmless for tooling, unacceptable for a data converter).

Dependencies of `lumen-common` on the shared-core side are intentionally tiny (`memchr`, `Cargo.toml:32`);
`lib.rs:1-4` says "Std and `memchr` only" even though the aim is `no_std + alloc` (the `lumen-html` crate is already
`#![no_std]`, `lib.rs:2`).

---

## 3. Should Lumen adopt a shared token IR?

### 3.1 Where a tape helps and where it does not

| Consumer | Needs | Tape benefit | Verdict |
|---|---|---|---|
| JS `JSON.parse` (no reviver) | full materialisation into `Value`/`Gc`, key interning to `Rc<str>`, last-wins duplicate keys, `__proto__` quirks, UTF-16 spelling | extra pass; array/object presizing is the only upside | **No** |
| JS `JSON.parse` + reviver | `context.source` per primitive (`RecordSink`, `json.rs:691`) | tape gives spans for free but the sink already records them | No |
| JS `JSON.stringify` | walks live objects with `toJSON`, replacer, getters, cycles | none (no document exists) | No, but writer ideas apply (4.2) |
| Python `json.loads` / `_json.scan_once` | `object_hook`, `parse_float`, `parse_int` callbacks at each node, CPython error text `(char N)` | none | No |
| Python `csv` | line-at-a-time feed from an iterator (`feed_line`) with dialect state; `_csv` semantics | none; tape would also break iterator-driven input | No |
| Python `pyexpat` | streaming events, partial feeds, expat error codes, DTD events, reparse deferral | none (needs SAX) | **No** |
| `xml.etree`/`minidom`/plist | built on pyexpat in pure Python | none | No |
| JS `DOMParser` (XML) and HTML | build `lumen_html::Document` arena with namespaces and a mutation journal | an intermediate tape duplicates the arena | No |
| `tsconfig.json`/`package.json` read | JSONC, tiny, one-shot | small allocation win vs `Value` tree | Marginal |
| Editing `package.json`/`pyproject.toml`/`tsconfig.json` preserving comments (`lumen add`, config migration) | trivia-preserving round-trip | **Yes**, this is RE:Dox's real niche | Only if the feature exists |
| Cross-format conversion (JSON <-> TOML <-> CBOR) | a neutral value model | yes, but need is speculative | Only if needed |
| Typed config deserialisation (Rust structs) | serde | serde already does it, no tape needed | No |
| Large-file lazy analytics (e.g. test262/html5lib JSON in tests) | skip most of the file | yes, but tests only | Marginal |

### 3.2 Why RE:Dox's headline wins do not carry over

- *Parallel deserialisation*: workers in Lumen cannot share an engine heap. `Value`s are `Rc`/GC-backed per realm
  (`lumen/src/builtins/json.rs` uses `Rc<str>` keys and `Gc` objects); `JsSink` borrows `&mut Interp`.
  You could parse the tape in parallel, but you cannot materialise JS/Python objects on other threads.
  It remains useful for Rust-side typed decode (e.g. config into structs), where serde + rayon over
  `serde_json::RawValue` slices is the cheaper route (check).
- *No DOM allocation*: already true via `Sink`.
- *Lazy decode*: engines need decoded values immediately.
- *Editing*: engines do not edit documents.

### 3.3 What a tape would cost Lumen

- A second JSON implementation next to `json.rs` (Lumen rule: one implementation per format), or a rewrite of the
  one that both engines and the fuzz target depend on, with CPython/ECMAScript error-position parity to
  re-prove.
- `Spelling::Utf16/CodePoints` (lone surrogate smuggling, `lumen-common/src/smuggle.rs`) means string decode is
  language-specific; a tape stores raw slices and so is fine, but every decode path then needs the same
  per-language care it has now.
- Trivia-preserving mutation on a tape is the complex part (REDox needs 3000 lines in `Document.Internal.cs` and
  the free-list/jump machinery). `toml_edit` already solves this for TOML (check below).

### 3.4 Recommendation

**Do not introduce a repo-wide `doc`/`tape` IR for the existing parsers.** Instead:

1. Keep `Sink`-driven parsers as the engine path (JSON, CSV, XML via `Handler`).
2. Improve `lumen_common::json` using RE:Dox's *scanner and writer* lessons (section 4).
3. Reserve a design for a small **`lumen_common::doc`** module (section 3.5) that is built only when a concrete
   editing/lazy/convert feature lands, starting with JSONC and delegating TOML to `toml_edit`.

### 3.5 If/when a tape is built: proposed layout

Keep it deliberately smaller and stricter than RE:Dox's.

```rust
// lumen-common/src/doc/tape.rs   (no_std + alloc, no OS calls)
#[repr(transparent)] #[derive(Copy, Clone, Eq, PartialEq)]
pub struct Tok(u64);   // 8 bytes

// bits 63..60 : Kind  (Null, False, True, Int, Float, Str, Key, Array, Map, Trivia, Ext, Free)
// bits 59..56 : Sub   (kind-specific: Str: Plain/Escaped/Single/Multiline;
//                      Int: Dec/Hex/Oct/Bin/Big; Float: Dec/Inf/NaN;
//                      Trivia: Ws/Line/Block/Comma/Colon; Ext: Table/Dotted/Cdata/...)
// scalar : bits 55..0  = offset:32 | len:24     (source-backed; len==0xFFFFFF => long form, see below)
// Array/Map: open token bits 55..28 = child count (28), bits 27..0 = reserved;
//            the *next* tape word is a `Skip` token holding `end_index: u32 | parent_delta: u32`
//            so skip-subtree and parent are O(1) both directions with no parent table.
// Ext: payload = index into `side: Vec<SideItem>` for edited strings/numbers/containers.
pub struct Tape<'src> { src: &'src [u8], toks: Vec<Tok>, side: Vec<Side>, free: u32 }
```

Differences from RE:Dox, each motivated above:
- Open container is followed by a fixed `Skip` word (end index + parent distance): fixes the "last child has
  `link = 0`" hole (1.1) and removes the O(n) parent table (1.9). Cost: 8 bytes per container.
- Offsets are `u32` and source length is capped (e.g. 4 GiB), and inputs above the cap fall back to the streaming
  `Sink` path (JSON has no document-size requirement in the engines).
- Keys are their own `Key` kind so a Map is `[Map][Skip] (Key Value)*` and CSV-with-header rows do not duplicate
  header tokens: use a `Rows`/`Columns` extension holding the header once.
- Number tokens carry a *classification* only (like RE:Dox) but also keep the `is_float`/`small_int` result the
  current `Number` has (`json.rs:285-300`), so most integers never reach `str::parse`.
- No `Utf16` string materialisation in the tape; the per-language adapter decodes with its `Spelling`
  (`json::Str` already does).
- Mutation layer is a separate type, `Edit<'t>`, that records (path, replacement) overlays and renders to bytes
  by splicing source slices (a "rope"/patch list), rather than turning tape slots into extended tokens. For
  config editing this is simpler: untouched bytes are copied verbatim, so comments, whitespace and number
  text survive without modelling trivia as tokens at all. Trivia tokens are only needed if you want to *query*
  or *move* comments; for edit-in-place use the span-patch approach (this is how `toml_edit` conceptually
  keeps decor/spans, check).

Module layout:

```
lumen-common/src/doc/
  mod.rs        Kind/Sub enums, Tok, Tape, Cursor (random-access by index), walk(&Tape, &mut impl Visit)
  scan.rs       structural scanning primitives (memchr-based, later optional SIMD)
  json.rs       JSON/JSONC/JSON5 front-end (reuses the current Parser's grammar; today's json.rs becomes
                the Sink adapter over the same scanner)
  edit.rs       span-patch edits + renderer
  convert.rs    Visit -> writer for neutral models (only the neutral value kinds)
```

Front-ends added only on demand. XML, HTML and CSV stay on their current cores; they get a *thin* optional
`Handler -> Tape` adapter (a `Handler` impl that allocates tokens) so that conversion/analysis can consume them,
instead of rewriting their scanners onto the tape.

---

## 4. Where RE:Dox's techniques beat today's code (measure-worthy only)

### 4.1 JSON scanning (`lumen-common/src/json.rs`)

Current behaviour (read): `string_body` loops byte by byte comparing to `"`, `\\`, `< 0x20` (`:640-654`); `ws`
loops per byte (`:393`); `number` is byte-wise with a 15-digit integer fast path (`:575-632`). Candidates:
1. `memchr::memchr2(b'"', b'\\', ..)` for the string body, then a vectorisable `all(|b| b >= 0x20)` pass on the span
   for strict mode (or one `memchr3` plus a SWAR control check). `memchr` is already a `lumen-common` dependency.
2. Whitespace skip with a 256-entry class table or a SWAR run skip for pretty-printed input.
3. Presize arrays/objects: `Sink::array()` has no size hint; REDox presizes the tape by `len/16`. Measure
   `Vec::with_capacity(8)` vs lazy growth for `JsSink` and `PySink`.
4. Quoting: `escape_into` branches per byte (`:81-130`). A 256-entry `needs_escape` table, or REDox's `SearchValues`
   idea (`JsonTextEncoder.cs:93`) via `memchr3` for `"`, `\\` plus a control check, with a bulk
   `push_str` for the clean run. Measure on string-heavy payloads (twitter.json) and on long ASCII strings.

What to benchmark: `canada.json` (number heavy, 2.2 MB), `citm_catalog.json`, `twitter.json` (same set as
`README.md` of REDox, so numbers are comparable in shape), against `serde_json::from_str::<Value>` and
`simd-json` as reference lines, at three levels: `lumen_common::json::Parser` with a no-op `Sink` (pure scan),
JS `JSON.parse` end to end, Python `json.loads` end to end. Also `JSON.stringify` / `json.dumps` of the parsed
values. Expectation: the scan portion improves modestly; the engine-materialisation portion dominates end to end,
which is the honest ceiling for any parser-level change.

### 4.2 `JSON.stringify` / `json.dumps`

REDox's writer wins (1.8) are fused name+value writes with pre-encoded property keys and bulk typed arrays. Lumen's
JS stringifier already builds into a single `String` (`lumen/src/builtins/json.rs:158-415`). Candidate: cache the quoted form
`"key":` per shape/hidden-class key (once per `Rc<str>` key atom) and append it in one `push_str`, skipping per-key
`quote_into`. Benchmark: array of 100k small objects with identical keys. Only worth it if profiling shows
`quote_into` on keys is > ~10% of stringify time.

### 4.3 Key/value strings

REDox does not intern; Lumen's `JsSink` does (per-parse `FastMap<&str, Rc<str>>`). Nothing to import.
A tape would make "intern after a first pass" possible (count distinct keys, presize the map) but that costs the
pass it saves; skip.

### 4.4 A tape only pays when a document is read more than once or partially

Mixed workloads: parse `package.json` once and query 5 fields; JSONC with many unused sections; very large
inputs where only a prefix is consumed. Benchmark against `Value` tree (`lumen_common::json::parse`): allocation
count and peak bytes. The expected tape gain is allocation, not time, and only for large inputs. For the tiny
config files Lumen reads, it is not measurable.

### 4.5 Where nothing here beats existing code

- `pyexpat`: streaming, expat error parity (45 codes, positions, DTD handlers). Do not remodel as tape.
- HTML: WHATWG tokenizer + tree builder (insertion modes, foster parenting, adoption agency). RE:Dox's HTML
  front-end is a "practical" scanner; spec-correct output is a Lumen requirement (html5lib conformance in
  `lumen-html/tests`).
- CSV: `_csv` is line-fed with CPython-specific quirks; REDox's CSV is whole-buffer strict.

---

## 5. Wrap maintained crates, or write the tape layer?

Lumen rules (`CLAUDE.md`): reuse first; never hand-write a well-specified format when a maintained crate
exists; prefer pure Rust and `no_std`-compatible. The two engines also have language-specific semantics that
generic crates will not match (CPython `_json` error positions, expat error codes, `_csv` dialect state).

| Need | Crate | Fit | Caveat |
|---|---|---|---|
| Fast JSON parse to tape/value | `simd-json` (tape + `Value`; AVX2/NEON, pure Rust) | Best raw throughput | Needs a mutable `&mut [u8]` (unescapes in place) so it cannot return borrowed `Cow<str>` into a `&str` source; error text/positions differ from CPython/V8; no UTF-16/lone-surrogate spelling; no JSONC; `std` (check). Use at most as an optional fast path for large inputs with fallback to the existing parser on error to keep error parity. |
| Lazy raw slices | `serde_json::value::RawValue` | Good for Rust-side config; keeps unparsed substrings | `std`; not a tape; no trivia. |
| JSON structural scan only | `memchr` (already used) / `core::simd` once stable | Cheapest first step | Handwritten glue remains. |
| Python-shaped fast JSON | `jiter` (Pydantic; iterator over tokens, lazy) (check) | Close to the `Sink` model, handles big ints | Own error model; Python-flavoured number types. |
| XML | `quick-xml` (pull reader, `NsReader`, `no_std`? not) | Faster, maintained | Does not give expat error codes/positions, DTD events or reparse deferral; keep `lumen-common/src/xml` for pyexpat. It could back a *new* lightweight consumer (e.g. SVG/XML data import) only. `lumen-html/src/xml.rs` is the one place where the choice is open, but it is being migrated to `lumen_common::xml`, so leave it. |
| CSV | `csv-core` (no_std state machine) | Good for plain RFC 4180 reading | `_csv` semantics (QUOTE_NOTNULL, escapechar, strict, `skipinitialspace`) are not covered; the current 463-line core is deliberately CPython-shaped. Only consider for a non-Python consumer. |
| TOML read/write with comments | `toml_edit` (`DocumentMut`, keeps decor/whitespace/comments, spans) | **Use it** for any pyproject/lumen.toml editing; `toml` 0.8 (in `lumen-cli`) is built on it | `std`; Rust-side only. For Python `tomllib` the vendored pure-Python module needs no native. |
| INI | none needed | CPython `configparser` is pure Python | Nothing to wrap. |
| CBOR | `ciborium` (serde), `minicbor` (no_std, `alloc`) | `minicbor` fits `no_std` rule | Only if a consumer appears (structured clone to disk? not today). |
| MessagePack | `rmp` / `rmpv` | maintained, pure Rust | Same: no consumer. |
| plist | `plist` crate (XML + binary) | maintained | Python's `plistlib.py` works unmodified on pyexpat + `struct`; native only if profiling says so. |
| Value model for conversion | `serde_json::Value`, `ciborium::Value`, `rmpv::Value` | exist | Each has its own number model; convert through one Lumen enum if needed. |

Is a thin wrapper around these better than a tape layer? For the *document model* the answer is yes for TOML
(`toml_edit`), probably yes for CBOR/MsgPack (`minicbor`/`rmp`, with a `Sink`-style adapter), and "keep what you
have" for JSON/CSV/XML. For *editable JSON/JSONC* no crate gives trivia-preserving edits with the right
semantics, which is the one place a small in-house span-patch editor (3.5) is justified, and it should reuse the
existing `json::Parser` grammar rather than add a scanner.

---

## 6. Where it should NOT be used

- Streaming SAX with partial feeds (`pyexpat.Parse(data, isfinal)`, `xml.sax`, `XMLPullParser`): needs resumable
  state and event ordering; a whole-document tape cannot answer the first event before the last byte.
- The WHATWG HTML tokenizer/tree builder (`lumen-html/src/html.rs`): modal, context-dependent, re-parenting,
  fragment contexts; the output is an arena DOM with identity and mutation records, not a tape.
- XML namespace-correct DOM for JS (`DOMParser`): needs prefix scoping, `xmlns` attribute semantics, entity
  rules; REDox itself skips these.
- Python `pickle`, `marshal`, `struct`: object-graph/binary codecs with memo and class lookups.
- `JSON.stringify`/`json.dumps`: no input document.
- Anything where the consumer wants engine values with identity (objects, hidden classes, GC): the sink writes
  directly into them.

---

## 7. Incremental migration order

0. (Zero code change) Decide that `lumen_common::json::{Parser, Sink, quote_into}` remains the single JSON core.
1. **Measure** (small): add a bench (criterion or a `cargo test --release`-style harness under
   `lumen-common`, with a timeout and the `fast` profile, per `CLAUDE.md`) that runs `Parser::document` over
   canada/citm/twitter with a no-op sink and a counting sink, and `escape_into` over the strings in them.
   Add `serde_json` and `simd-json` as dev-dependency baselines only. Record numbers in a doc.
2. **Scan optimisations** in `json.rs`: memchr-based `string_body`, table-driven `ws`, table/`memchr3`-based
   `escape_into`. Re-run step 1, the `json_parse` fuzz target, the CPython corpus (`cargo test -p lumen-py --test
   corpus`), test262 `built-ins/JSON`. Merge if >= 15-20% on the scan-only bench and no end-to-end regression
   (otherwise revert; the end-to-end number is the one that counts).
3. **Sink hints**: optional `fn array_hint`/`object_hint` or use of a first-pass `Skip` count only if step 2 shows
   `Vec` growth is a visible cost in `JsSink`/`PySink`.
4. **Stringify key cache** (4.2), only if profiling justifies.
5. **Config editing** (if a feature is scheduled): `toml_edit` for TOML in `lumen-cli`; for JSON/JSONC either a
   span-patch editor in `lumen_common::doc::edit` over the existing `json::Parser` grammar (extended with a
   `Sink` that records spans) or defer.
6. **Tape** only after (5) shows a second consumer that wants lazy or parallel access. Then implement
   `lumen_common::doc::{Tok, Tape}` from 3.5 with a JSON front-end first, validated by differential tests against
   `json::parse` on the fuzz corpus (same accept/reject set, same strings and numbers).
7. **Other formats**: CBOR/MsgPack through `minicbor`/`rmp` behind a `Sink`-shaped adapter if a consumer appears;
   YAML only if a Lumen feature needs it. INI/TOML for Python come from vendored pure-Python modules; nativise
   `tomllib` only if import-time or parse-time profiling of a real workload (e.g. a large `pyproject.toml`) hurts.
8. **Duplication review** (per `CLAUDE.md`) on whichever branch lands first: confirm no second JSON/XML grammar was
   added (e.g. in `lumen-html/src/xml.rs` vs `lumen_common::xml`, or `serde_json::Value` vs `json::Value`
   in `lumen-cli`, which could move to `json::parse` + a shared Value and drop `serde_json` from tooling paths
   where numbers > 2^53 do not matter).

### Adjacent duplication noticed (not part of this question)

- `lumen-cli` carries `serde_json` + `toml`, while the rest of the workspace uses `lumen_common::json::Value`.
  Two JSON trees in tooling. Low priority.
- `lumen-html/src/xml.rs` (own XML grammar) vs `lumen-common/src/xml/` (expat clone): already being merged by
  another agent. When it lands, the doc/arena builder should be a `Handler` implementation in `lumen-html`,
  and the 4 MiB / depth 512 / node-count caps should be expressed through the shared parser's `Options`/`Shared`
  accounting (`lumen-common/src/xml/mod.rs:208-262`), not re-implemented.

## 8. Cost/benefit summary

| Option | Cost | Expected benefit | Recommend |
|---|---|---|---|
| General shared token IR for all formats | Large (new core, 9 front-ends, parity proofs for 2 engines) | Small for engines; real only for editing/convert | No |
| Scan/escape optimisation in `json.rs` | Small | Measurable on scan-bound payloads | **Yes, first** |
| Stringify key cache | Small | Conditional on profile | Maybe |
| `toml_edit` for config editing | Small, wrapper only | Complete trivia fidelity for free | Yes, when needed |
| Span-patch JSONC editor | Medium | Fills the one gap no crate covers | Yes, when needed |
| Tape (3.5) as lazy/parallel substrate | Medium-large | Only for Rust-side bulk analytics | Defer |
| `simd-json` as a large-input fast path | Medium, parity risk | High raw throughput, low end-to-end share | Only after step 1 shows scan share is large |
| Replace expat clone with `quick-xml` | Medium, loses expat parity | Speed only | No |
| CBOR/MsgPack/YAML | Small via crates | No consumer | On demand |
