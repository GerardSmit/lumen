# Generated glue modules

`src/js/net.js`, `src/js/url.js` and `src/js/fs.js` are generated: each wraps Node's own
`lib/` sources (MIT, Copyright Node.js contributors; the vendored copies keep their headers) in
lumen's module table, with small `lumen:`-marked patches applied by the generator. Edit the
generator inputs here, never the generated file, then regenerate from the repository root:

    node crates/lumen-node/tools/netgen/gen.js crates/lumen-node/src/js/net.js
    node crates/lumen-node/tools/urlgen/gen.js crates/lumen-node/src/js/url.js
    node crates/lumen-node/tools/fsgen/gen.js  crates/lumen-node/src/js/fs.js

| generator | shell (`head.js`/`tail.js`) | vendored Node sources            |
|-----------|-----------------------------|----------------------------------|
| `netgen`  | `netgen/`, plus `netgen/http_parser.js` (llhttp port) | `netgen/node/` (Node 20.11) |
| `urlgen`  | `urlgen/` (+ `constants.js`, also used by `netgen`)  | `nodeurl/`, `nodefs/internal_url.js` |
| `fsgen`   | `fsgen/`                    | `nodefs/` (Node 20.10)           |

The output is deterministic; a regenerated file must be byte-identical to the checked-in one
unless an input changed.
