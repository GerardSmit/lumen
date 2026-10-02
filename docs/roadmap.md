# Roadmap

State of `worktree-lumen-python` and what is left. Branches named below exist locally and are not
merged yet.

## Done

- `lumen-bind`: one language-neutral binding framework; every JS and Python native goes through it
  (`ops!`/`OpDecl` removed).
- Python blocker work merged: decimal, compression/xml, OS modules, ssl, testcapi, tracing (PEP 669),
  cycle GC, GIL threads, docstrings.
- Embedded stdlib: Brotli-compressed and comment-stripped (4.24 MB to 0.69 MB; binary 11.7 to 8.5 MB).
- Crypto: `lumen-crypto` with system backends (Security.framework, CNG), OpenSSL, RustCrypto fallback.
- Duplication pass: shared rounding, PEM, UTF stepping, decompressor core.

Test state at merge: Python corpus 329/329, CPython 3.12 suite ok 159/434 files (25,917 tests
passing), node-compat 2761/3263, test262 smoke 2739/2739.

## Merged, partially done

- CPython suite fixes (`py-fixes`): `code.replace`, ssl record reads, yield-from resume, traceback
  chain cache, exception context cycles, non-blocking fds. CPython suite not rerun since.
- Python 3.14 (switch in progress): CPython 3.14.8 Lib vendored, version reporting; 3.13/3.14
  builtins, docstring dedent, PEP 765/758, `__static_attributes__`; `posix`/`fcntl`/`termios`/
  `mmap`/`select` at 3.14; `_opcode`, `_tokenize`, `marshal`, `_sre`; ~40 small native additions.
  13 corpus files are listed in `expected-failures.txt`: 12 wait for t-strings (`annotationlib`
  uses `t""`), one test was written for 3.12 opcodes.
- `dir_fd` is emulated through the directory path (racy); real `*at` calls are needed.
- OS-native TLS: uncommitted work in the `tls-system` worktree (stopped).

## Next

1. Python 3.14 (Home Assistant and Hermes need it)
   - Language (first: t-strings, it unblocks `annotationlib`, `typing`, `dataclasses`, `asyncio`
     and the real-world corpus): PEP 649/749 deferred annotations (`__annotate__`, `annotationlib`), PEP 750 t-strings
     (`string.templatelib`), PEP 667 `locals()`/`FrameLocalsProxy`, PEP 696 type parameter defaults.
   - Natives: `_zstd` (`compression.zstd`, on lumen-common `compress`), `_interpreters` and queues
     (on the existing sub-interpreters and `lumen_os::channel::Queue`), `_hmac`, 3.14 changes to
     `_hashlib`/`_ssl`/`_socket`, Unicode 16.0 UCD, `_ast` for 3.14, native `OrderedDict`.
   - Switch `scripts/cpython-fetch.sh` and the runner to 3.14 and write a new baseline.
2. CPython suite failures: fan out per failure cluster; timeouts under parallel load, `test_bz2` and
   `test_resource` crashes, `test_hash` flakiness, ftplib/poplib TLS hangs.
3. Real-world programs: Home Assistant, Hermes plugins, OpenClaw, LLM SDKs (openai, anthropic,
   httpx, pydantic).
4. OS-native TLS (Network.framework / SChannel, OpenSSL and rustls fallback); `lumen-tls` then uses
   `lumen_common::pem` and drops its second OpenSSL binding table.
5. Hand-written code to replace with crates: `sha-crypt`, `httparse`, `wasmparser`, Bun hashes
   (`xxhash`/`wyhash`/`rapidhash`), `unicode-normalization` for JS. xz stays hand-written by choice.
6. Shared line editor for Python `readline`/`_pyrepl`/`_curses` and the Node REPL.
7. Known pre-existing issues: `bun_postgres` SCRAM test hangs on macOS; 70 `lumen-runtime` tests
   need a large stack in debug builds; `test-tls-zero-clear-in` times out.
