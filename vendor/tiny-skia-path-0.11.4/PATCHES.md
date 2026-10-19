This is the exact crates.io `tiny-skia-path` 0.11.4 source release.
The upstream license files are preserved.

Local patches: a `BoundedPathBuilder` with a shared live-allocation budget
(`PathAllocationBudget`, `StrokeBudgetExceeded`), `Path::allocated_bytes`, and
budget checks in the stroker, so page-controlled SVG and canvas paths cannot
allocate without limit. Also drops unused `NoStdFloat` imports and tidies a
`PathSegmentsIter<'_>` lifetime. Used by `lumen-common::svg_path`.
