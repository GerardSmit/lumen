use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub fn features(inputs: &[PathBuf], imports: &[String], profile: &str, trimmed: bool, has_locales: bool) -> BTreeSet<String> {
    let mut features = BTreeSet::from([profile.to_owned()]);
    let optional = ["bun", "http2", "cluster", "dgram", "wasi", "parallel", "intl"];
    if !trimmed { features.extend(optional.iter().map(|name| (*name).to_owned())); }
    for import in imports {
        let name = import.strip_prefix("node:").unwrap_or(import);
        let feature = match name {
            "http2" => Some("http2"), "cluster" => Some("cluster"), "dgram" => Some("dgram"),
            "wasi" => Some("wasi"), "lumen:parallel" => Some("parallel"),
            name if name.starts_with("bun:") => Some("bun"), _ => None,
        };
        if let Some(feature) = feature { features.insert(feature.to_owned()); }
    }
    if has_locales || inputs.iter().filter_map(|path| std::fs::read_to_string(path).ok())
        .any(|source| ["Intl", "toLocale", "globalThis", "Reflect"].iter().any(|name| source.contains(name))) {
        features.insert("intl".into());
    }
    features
}

pub fn write(output: &Path, inputs: &[PathBuf], imports: &[String], profile: &str,
    trimmed: bool, locales: Option<&[String]>, target_os: &str) -> Result<(), String> {
    let path = PathBuf::from(format!("{}.runtime.json", output.display()));
    if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    if path.canonicalize().is_ok_and(|path| inputs.iter().any(|input| input.canonicalize().is_ok_and(|input| input == path))) {
        return Err("runtime build plan would overwrite an input".into());
    }
    let features = features(inputs, imports, profile, trimmed, locales.is_some());
    let mut locale_set: BTreeSet<String> = locales.unwrap_or_default().iter().map(|locale| locale.to_ascii_lowercase()).collect();
    if locales.is_some() { locale_set.insert("en".into()); }
    let features = features.into_iter().collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "format": 1, "package": "lumen-exe-runtime", "defaultFeatures": false,
        "profile": profile, "os": target_os, "features": features,
        "environment": locales.map(|_| serde_json::json!({"LUMEN_LOCALES": locale_set.into_iter().collect::<Vec<_>>().join(",")})),
        "requiredModules": imports,
        "cargoArguments": ["build", "--release", "-p", "lumen-exe-runtime", "--no-default-features", "--features", features.join(",")],
        "notes": ["Rebuild the selected runtime with this feature and locale selection before linking or packaging.",
            "Core engine and host builtin groups currently remain linked; this plan selects existing optional Cargo groups."]
    });
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?)
        .map_err(|error| format!("{}: {error}", path.display()))
}

pub struct Variant {
    pub descriptor: PathBuf,
    pub stub: Option<PathBuf>,
    pub library: Option<PathBuf>,
}

pub fn select(catalog: &Path, os: &str, arch: &str, profile: &str, mode: &str,
    required: &BTreeSet<String>, locales: Option<&[String]>) -> Result<Variant, String> {
    let bytes = std::fs::read(catalog).map_err(|error| format!("{}: {error}", catalog.display()))?;
    let catalog_value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let root = catalog.parent().unwrap_or(Path::new("."));
    let object = catalog_value.as_object().ok_or("runtime catalog must be an object")?;
    if object.len() != 2 || object.get("format").and_then(serde_json::Value::as_u64) != Some(1) { return Err("runtime catalog requires format 1 and variants".into()); }
    let mut candidates = Vec::new();
    for value in object.get("variants").and_then(serde_json::Value::as_array).ok_or("runtime catalog variants must be an array")? {
        let variant = value.as_object().ok_or("runtime variant must be an object")?;
        if variant.keys().any(|key| !matches!(key.as_str(), "os" | "arch" | "profile" | "features" | "locales" | "descriptor" | "stub" | "runtimeLib")) {
            return Err("unknown runtime variant field".into());
        }
        let text = |key: &str| variant.get(key).and_then(serde_json::Value::as_str).ok_or_else(|| format!("runtime variant requires {key}"));
        let variant_os = text("os")?;
        let variant_arch = text("arch")?;
        let variant_profile = text("profile")?;
        if !matches!(variant_os, "windows" | "linux" | "macos") || !matches!(variant_arch, "x86_64" | "aarch64")
            || !matches!(variant_profile, "full" | "nojit" | "aot") { return Err("invalid runtime variant platform/profile".into()); }
        let list = |key: &str| -> Result<Option<BTreeSet<String>>, String> {
            variant.get(key).map(|value| value.as_array().ok_or_else(|| format!("runtime {key} must be an array"))?
                .iter().map(|value| value.as_str().map(str::to_owned).ok_or_else(|| format!("runtime {key} entries must be strings")))
                .collect::<Result<BTreeSet<_>, _>>()).transpose()
        };
        let mut features = list("features")?.ok_or("runtime variant requires features")?;
        if features.iter().any(|feature| matches!(feature.as_str(), "full" | "nojit" | "aot") && feature != variant_profile)
            || (variant_profile == "aot" && features.iter().any(|feature| matches!(feature.as_str(), "compiler" | "jit" | "typed")))
            || (variant_profile == "nojit" && features.contains("jit")) {
            return Err("runtime variant features contradict its profile".into());
        }
        features.insert(variant_profile.into());
        if variant_profile != "aot" { features.extend(["intl".into(), "typed".into()]); }
        let available_locales = list("locales")?;
        if available_locales.as_ref().is_some_and(|locales| locales.is_empty() || locales.iter().any(|locale| !crate::aot_config::valid_locale(locale))) {
            return Err("runtime variant locales must contain valid BCP47 tags".into());
        }
        let locale_match = match (locales, &available_locales) {
            (None, None) => true,
            (Some(requested), Some(available)) => requested.iter().all(|locale| available.iter().any(|candidate| candidate.eq_ignore_ascii_case(locale.split('-').next().unwrap_or(locale)))),
            (Some(_), None) => true,
            _ => false,
        };
        if variant_os != os || variant_arch != arch || variant_profile != profile || !required.is_subset(&features) || !locale_match { continue; }
        let descriptor = root.join(text("descriptor")?);
        let path = |key: &str| -> Result<Option<PathBuf>, String> { variant.get(key).map(|value| value.as_str().map(|value| root.join(value)).ok_or_else(|| format!("runtime {key} must be a path"))).transpose() };
        let selection = Variant { descriptor, stub: path("stub")?, library: path("runtimeLib")? };
        if (mode == "link" && selection.library.is_none()) || (mode == "stub" && selection.stub.is_none()) { continue; }
        let bytes = std::fs::read(&selection.descriptor).map_err(|error| format!("{}: {error}", selection.descriptor.display()))?;
        let target = lumen::target::TargetSpec::decode(&bytes)?;
        let expected_arch = if arch == "aarch64" { lumen::target::Arch::Aarch64 } else { lumen::target::Arch::X86_64 };
        let expected_profile = match profile { "aot" => lumen::target::Profile::Aot, "nojit" => lumen::target::Profile::NoJit, _ => lumen::target::Profile::Full };
        let expected_abi = match (os, expected_arch) {
            ("windows", _) => lumen::target::Abi::Win64,
            ("macos", lumen::target::Arch::Aarch64) => lumen::target::Abi::Apple64,
            ("linux", lumen::target::Arch::Aarch64) => lumen::target::Abi::Aapcs64,
            _ => lumen::target::Abi::SysV64,
        };
        if target.arch != expected_arch || target.abi != expected_abi || target.profile != expected_profile {
            return Err(format!("{}: runtime descriptor contradicts catalog platform/profile", selection.descriptor.display()));
        }
        let artifact = if mode == "link" { selection.library.as_ref().unwrap() } else { selection.stub.as_ref().unwrap() };
        if !artifact.is_file() { return Err(format!("{}: runtime artifact is missing", artifact.display())); }
        candidates.push((features.len(), available_locales.as_ref().map_or(usize::MAX, BTreeSet::len), selection.descriptor.clone(), selection));
    }
    candidates.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    candidates.into_iter().next().map(|(_, _, _, variant)| variant).ok_or_else(|| format!("no runtime variant satisfies {os}/{arch}/{profile}, {mode}, features {required:?} and locales {locales:?}"))
}
