This is the exact crates.io `image` 0.25.10 source release.
The upstream license files are preserved.

Local patches:
- `src/imageops/sample.rs`: `GaussianBlurParameters::new_anisotropic_sigma`
  and `kernel_sizes`, so SVG `feGaussianBlur` can blur the two axes with
  independent standard deviations; a width-1 kernel is the identity.
- `src/imageops/filter_1d.rs`: singleton-kernel handling skips the arena's
  leading pixel and avoids the ring-queue path for a vertical kernel of
  length 1 (upstream produced shifted output).
- `src/codecs/png.rs`, `src/utils/mod.rs`: `#[cfg(feature = ...)]` guards
  that silence unused-code warnings in Lumen's feature set.
The first two are candidates for upstreaming.
