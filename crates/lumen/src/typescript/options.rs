//! The compiler options the typed tier honours (docs/typed-tier.md §4.6). Reading them from a
//! `tsconfig.json` (JSONC, `extends`) is the host's job: `lumen_runtime::tsconfig`.

use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompilerOptions {
    /// Off: the whole file is hints-only (row 18).
    pub strict_null_checks: bool,
    pub no_implicit_any: bool,
    /// Honoured: element reads are `T | undefined` and need no out-of-bounds exit (row 7).
    pub no_unchecked_indexed_access: bool,
    pub exact_optional_property_types: bool,
    /// Off: class layouts are hints-only (the strip always produces `[[Define]]` fields).
    pub use_define_for_class_fields: bool,
    /// Enables JSDoc checking for `.js` files (§4.5).
    pub check_js: bool,
    pub allow_js: bool,
    /// The `tsconfig.json` these came from; `None` means lumen's defaults.
    pub source: Option<PathBuf>,
}

impl Default for CompilerOptions {
    /// Lumen's defaults when there is no tsconfig: strict on, `noUncheckedIndexedAccess` off.
    fn default() -> Self {
        CompilerOptions {
            strict_null_checks: true,
            no_implicit_any: true,
            no_unchecked_indexed_access: false,
            exact_optional_property_types: false,
            use_define_for_class_fields: true,
            check_js: false,
            allow_js: false,
            source: None,
        }
    }
}
