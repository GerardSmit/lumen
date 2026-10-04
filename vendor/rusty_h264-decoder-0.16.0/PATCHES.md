This is the exact crates.io `rusty_h264-decoder` 0.16.0 source release,
corresponding to upstream commit `63b527c4019a6584eb30d8b181b62ba085c04fef`.
The upstream BSD-2-Clause license is preserved in `LICENSE`.

Local patch: `src/mb16.rs`'s nested `edcstat` module uses `Vec` in its report
function but did not import `alloc::vec::Vec`. The missing import prevents the
crate's documented `no_std` + `alloc` configuration from compiling. No decoder
behavior or upstream pin is otherwise changed.

Upstream: https://github.com/Remade-With-Rust/rusty_h264/tree/63b527c4019a6584eb30d8b181b62ba085c04fef/crates/rusty_h264-decoder
