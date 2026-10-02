//! `tsconfig.json` reader (docs/typed-tier.md §4.6, milestone F4): JSONC (comments and
//! trailing commas), `extends` (a path, a package, or an array of them), and the options the
//! typed tier honours.

use std::path::{Path, PathBuf};

use lumen_host::sysfs::PathExt as _;

pub use lumen::typescript::CompilerOptions;

/// The raw option values of one config file chain (later files override earlier ones).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawOptions {
    pub strict: Option<bool>,
    pub strict_null_checks: Option<bool>,
    pub no_implicit_any: Option<bool>,
    pub no_unchecked_indexed_access: Option<bool>,
    pub exact_optional_property_types: Option<bool>,
    pub use_define_for_class_fields: Option<bool>,
    pub check_js: Option<bool>,
    pub allow_js: Option<bool>,
    pub target: Option<String>,
}

impl RawOptions {
    fn merge_from(&mut self, over: &RawOptions) {
        macro_rules! take {
            ($($f:ident),*) => { $( if over.$f.is_some() { self.$f = over.$f.clone(); } )* };
        }
        take!(
            strict,
            strict_null_checks,
            no_implicit_any,
            no_unchecked_indexed_access,
            exact_optional_property_types,
            use_define_for_class_fields,
            check_js,
            allow_js,
            target
        );
    }

    /// Applies TypeScript's defaults for a project that has a tsconfig.
    pub fn resolve(&self, source: Option<PathBuf>) -> CompilerOptions {
        let strict = self.strict.unwrap_or(false);
        // `useDefineForClassFields` defaults to true only for ES2022+ / ESNext targets.
        let modern_target = self.target.as_deref().is_some_and(|t| {
            let t = t.to_ascii_lowercase();
            t == "esnext"
                || t.strip_prefix("es")
                    .and_then(|y| y.parse::<u32>().ok())
                    .is_some_and(|y| y >= 2022)
        });
        CompilerOptions {
            strict_null_checks: self.strict_null_checks.unwrap_or(strict),
            no_implicit_any: self.no_implicit_any.unwrap_or(strict),
            no_unchecked_indexed_access: self.no_unchecked_indexed_access.unwrap_or(false),
            exact_optional_property_types: self.exact_optional_property_types.unwrap_or(false),
            use_define_for_class_fields: self.use_define_for_class_fields.unwrap_or(modern_target),
            check_js: self.check_js.unwrap_or(false),
            allow_js: self.allow_js.unwrap_or(false),
            source,
        }
    }
}

pub use lumen_common::json::Value as Json;

/// Parses JSONC: JSON plus `//` and `/* */` comments and trailing commas.
pub fn parse_jsonc(text: &str) -> Result<Json, String> {
    lumen_common::json::parse_jsonc(text).map_err(|e| e.to_string())
}

fn raw_from(json: &Json) -> RawOptions {
    let mut raw = RawOptions::default();
    let Some(opts) = json.get("compilerOptions") else {
        return raw;
    };
    let b = |k: &str| match opts.get(k) {
        Some(Json::Bool(v)) => Some(*v),
        _ => None,
    };
    raw.strict = b("strict");
    raw.strict_null_checks = b("strictNullChecks");
    raw.no_implicit_any = b("noImplicitAny");
    raw.no_unchecked_indexed_access = b("noUncheckedIndexedAccess");
    raw.exact_optional_property_types = b("exactOptionalPropertyTypes");
    raw.use_define_for_class_fields = b("useDefineForClassFields");
    raw.check_js = b("checkJs");
    raw.allow_js = b("allowJs");
    if let Some(Json::Str(t)) = opts.get("target") {
        raw.target = Some(t.clone());
    }
    raw
}

fn resolve_extends(base_dir: &Path, spec: &str) -> Option<PathBuf> {
    let with_json = |p: PathBuf| -> Option<PathBuf> {
        if p.fs_is_file() {
            return Some(p);
        }
        let mut s = p.clone().into_os_string();
        s.push(".json");
        let j = PathBuf::from(s);
        if j.fs_is_file() {
            return Some(j);
        }
        let t = p.join("tsconfig.json");
        t.fs_is_file().then_some(t)
    };
    let path = Path::new(spec);
    if spec.starts_with('.') || path.is_absolute() {
        return with_json(base_dir.join(path));
    }
    // A package: look in node_modules up the directory chain.
    let mut dir = Some(base_dir);
    while let Some(d) = dir {
        if let Some(p) = with_json(d.join("node_modules").join(spec)) {
            return Some(p);
        }
        dir = d.parent();
    }
    None
}

fn load_raw(path: &Path, depth: usize) -> Result<RawOptions, String> {
    if depth > 32 {
        return Err("tsconfig `extends` chain too deep".into());
    }
    let text = lumen_host::sysfs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let json = parse_jsonc(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut raw = RawOptions::default();
    let specs: Vec<String> = match json.get("extends") {
        Some(Json::Str(s)) => vec![s.clone()],
        Some(Json::Arr(items)) => items
            .iter()
            .filter_map(|i| match i {
                Json::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    for spec in specs {
        let base = resolve_extends(dir, &spec)
            .ok_or_else(|| format!("{}: cannot resolve extends '{spec}'", path.display()))?;
        raw.merge_from(&load_raw(&base, depth + 1)?);
    }
    raw.merge_from(&raw_from(&json));
    Ok(raw)
}

/// Loads one tsconfig file (following `extends`).
pub fn load(path: &Path) -> Result<CompilerOptions, String> {
    Ok(load_raw(path, 0)?.resolve(Some(path.to_path_buf())))
}

/// Parses tsconfig text without following `extends`.
pub fn parse(text: &str) -> Result<CompilerOptions, String> {
    Ok(raw_from(&parse_jsonc(text)?).resolve(None))
}

/// The options for `file`: the nearest `tsconfig.json` in its directory or an ancestor, or
/// lumen's defaults if there is none.
pub fn load_for(file: &Path) -> Result<CompilerOptions, String> {
    let abs = if file.is_absolute() {
        file.to_path_buf()
    } else {
        lumen_host::sysfs::current_dir()
            .map_err(|e| e.to_string())?
            .join(file)
    };
    let mut dir = abs.parent();
    while let Some(d) = dir {
        let candidate = d.join("tsconfig.json");
        if candidate.fs_is_file() {
            return load(&candidate);
        }
        dir = d.parent();
    }
    Ok(CompilerOptions::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsconfig_jsonc_and_defaults() {
        let o = parse(
            r#"{
            // comment
            "compilerOptions": {
                /* block */ "strict": true,
                "noUncheckedIndexedAccess": true,
                "target": "ES2022",
            },
        }"#,
        )
        .unwrap();
        assert!(
            o.strict_null_checks && o.no_unchecked_indexed_access && o.use_define_for_class_fields
        );
        let o = parse(r#"{ "compilerOptions": { "target": "es2017" } }"#).unwrap();
        assert!(
            !o.strict_null_checks,
            "strict defaults to off with a tsconfig"
        );
        assert!(
            !o.use_define_for_class_fields,
            "define defaults to target >= ES2022"
        );
        let o = parse(
            r#"{ "compilerOptions": { "strict": true, "strictNullChecks": false, "checkJs": true } }"#,
        )
        .unwrap();
        assert!(!o.strict_null_checks && o.check_js && o.no_implicit_any);
        assert!(parse("{ \"compilerOptions\": ").is_err());
    }

    #[test]
    fn tsconfig_extends_chain() {
        let dir = std::env::temp_dir().join(format!("lumen-ts-tsconfig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/@acme/cfg")).unwrap();
        std::fs::write(
            dir.join("node_modules/@acme/cfg/tsconfig.json"),
            r#"{ "compilerOptions": { "strict": true, "target": "esnext" } }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("base.json"),
            r#"{ "extends": "@acme/cfg/tsconfig.json", "compilerOptions": { "noUncheckedIndexedAccess": true } }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("sub/tsconfig.json"),
            r#"{ "extends": ["../base.json"], "compilerOptions": { "strictNullChecks": false } }"#,
        )
        .unwrap();
        std::fs::write(dir.join("sub/a.ts"), "export {};").unwrap();
        let o = load_for(&dir.join("sub/a.ts")).unwrap();
        assert!(
            o.no_unchecked_indexed_access && o.use_define_for_class_fields && o.no_implicit_any
        );
        assert!(!o.strict_null_checks);
        assert_eq!(
            o.source.as_deref(),
            Some(dir.join("sub/tsconfig.json").as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
