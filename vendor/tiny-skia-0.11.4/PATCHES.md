This is the exact crates.io `tiny-skia` 0.11.4 source release.
The upstream license files are preserved.

Local patches: budgeted rasterisation entry points (`RasterBudgetExceeded`,
`Pixmap::fill_path_bounded`, `Pixmap::stroke_path_bounded`,
`Mask::rectangle_path_scan_bytes`, `Mask::fill_rectangle_path_bounded`,
`Shader::allocated_bytes`) with budget accounting in the edge builder, alpha
runs, scan converters, pipeline blitter and gradient/pattern shaders, so
page-controlled paths are rejected before they exhaust memory. The unbounded
upstream APIs keep their behaviour. Depends on the patched `tiny-skia-path`.
