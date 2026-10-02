//! Project defaults for `compile`. The first declared configuration wins.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const KEYS: &[&str] = &[
    "entry",
    "tier",
    "profile",
    "trim",
    "keep",
    "locales",
    "keepSource",
    "snapshotAt",
    "compression",
    "stripLines",
    "profileData",
    "assets",
    "targets",
    "signing",
    "output",
    "script",
    "nodeModules",
    "modules",
    "sourceRoot",
    "builtinCatalog",
    "runtimeCatalog",
];

#[derive(Default)]
pub struct Config {
    pub project_root: Option<PathBuf>,
    pub file: Option<PathBuf>,
    pub entry: Option<PathBuf>,
    pub tier: Option<String>,
    pub profile: Option<String>,
    pub profile_data: Option<PathBuf>,
    pub snapshot_at: Option<String>,
    pub signing_key: Option<String>,
    pub builtin_catalog: Option<PathBuf>,
    pub output: Option<PathBuf>,
    pub script: bool,
    pub node_modules: bool,
    pub keep_source: Vec<String>,
    pub modules: Vec<PathBuf>,
    pub source_root: Option<PathBuf>,
    pub compression: Option<bool>,
    pub trim_modules: bool,
    pub trim_set: bool,
    pub trim_level: Option<String>,
    pub keep: Vec<String>,
    pub strip_lines: bool,
    pub unsupported: Vec<String>,
    pub targets: std::collections::BTreeMap<String, NamedTarget>,
    pub assets: Vec<String>,
    pub locales: Option<Vec<String>>,
    pub runtime_catalog: Option<PathBuf>,
}

#[derive(Default)]
pub struct NamedTarget {
    pub output: Option<PathBuf>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub mode: Option<String>,
    pub device: Option<String>,
    pub descriptor: Option<PathBuf>,
    pub executable: bool,
    pub gui: bool,
    pub icon: Option<PathBuf>,
    pub stub: Option<PathBuf>,
    pub runtime_lib: Option<PathBuf>,
}

pub fn load(explicit: Option<&str>) -> Result<Config, String> {
    let selected = if let Some(path) = explicit {
        Some(PathBuf::from(path))
    } else {
        let mut selected = None;
        for name in ["package.json", "pyproject.toml", "lumen.json", "lumen.toml"] {
            let path = PathBuf::from(name);
            if declared(&path)? {
                selected = Some(path);
                break;
            }
        }
        selected
    };
    let Some(path) = selected else {
        return Ok(Config::default());
    };
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let value = if path.extension().is_some_and(|ext| ext == "toml") {
        let value: toml::Value =
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        serde_json::to_value(value).map_err(|e| format!("{}: {e}", path.display()))?
    } else if path.extension().is_some_and(|ext| ext == "json") {
        serde_json::from_str::<Value>(&raw).map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        return Err(format!(
            "{}: expected a JSON or TOML configuration",
            path.display()
        ));
    };
    let config = if path.file_name().is_some_and(|name| name == "package.json") {
        value
            .get("lumen")
            .ok_or("package.json has no lumen configuration")?
    } else if path
        .file_name()
        .is_some_and(|name| name == "pyproject.toml")
    {
        value
            .get("tool")
            .and_then(|tool| tool.get("lumen"))
            .ok_or("pyproject.toml has no [tool.lumen] configuration")?
    } else {
        &value
    };
    let object = config
        .as_object()
        .ok_or("lumen configuration must be a table/object")?;
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut parsed = parse(object, root)?;
    parsed.file = Some(path.clone());
    if parsed.entry.is_none() && path.file_name().is_some_and(|name| name == "package.json") {
        parsed.entry = ["main", "module", "bin"]
            .into_iter()
            .find_map(|key| {
                value.get(key).and_then(|entry| {
                    entry.as_str().or_else(|| {
                        (key == "bin")
                            .then(|| entry.as_object())
                            .flatten()
                            .filter(|map| map.len() == 1)
                            .and_then(|map| map.values().next())
                            .and_then(Value::as_str)
                    })
                })
            })
            .map(|entry| root.join(entry));
    }
    Ok(parsed)
}

fn declared(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    if !matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some("package.json" | "pyproject.toml")
    ) {
        return Ok(true);
    }
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if path.extension().is_some_and(|ext| ext == "json") {
        let value: Value =
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(value.get("lumen").is_some())
    } else {
        let value: toml::Value =
            toml::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(value
            .get("tool")
            .and_then(|tool| tool.get("lumen"))
            .is_some())
    }
}

fn parse(object: &Map<String, Value>, root: &Path) -> Result<Config, String> {
    let mut result = Config::default();
    result.project_root = Some(root.to_owned());
    for (key, value) in object {
        let key = if key.contains('_') {
            snake_to_camel(key)
        } else {
            key.clone()
        };
        if !KEYS.contains(&key.as_str()) {
            let closest = KEYS.iter().min_by_key(|known| distance(&key, known));
            return Err(format!(
                "unknown lumen configuration key `{key}`{}",
                closest
                    .filter(|known| distance(&key, known) <= 3)
                    .map_or(String::new(), |known| format!("; did you mean `{known}`?"))
            ));
        }
        match key.as_str() {
            "entry" => result.entry = Some(root.join(string(value, &key)?)),
            "tier" => {
                let tier = string(value, &key)?;
                if !matches!(tier, "bc" | "mc") {
                    return Err("tier must be bc or mc".into());
                }
                result.tier = Some(tier.to_owned());
            }
            "profile" => {
                let profile = string(value, &key)?;
                if !matches!(profile, "full" | "nojit" | "aot") {
                    return Err("profile must be full, nojit or aot".into());
                }
                result.profile = Some(profile.to_owned());
            }
            "output" => result.output = Some(root.join(string(value, &key)?)),
            "sourceRoot" => result.source_root = Some(root.join(string(value, &key)?)),
            "script" => result.script = boolean(value, &key)?,
            "nodeModules" => result.node_modules = boolean(value, &key)?,
            "keepSource" => result.keep_source = strings(value, &key)?,
            "modules" => {
                result.modules = strings(value, &key)?
                    .into_iter()
                    .map(|path| root.join(path))
                    .collect()
            }
            "signing" => {
                let signing = value.as_object().ok_or("signing must be an object")?;
                if signing.keys().any(|name| name != "key") {
                    return Err("signing only accepts key".into());
                }
                if let Some(key) = signing.get("key") {
                    let key = string(key, "signing.key")?;
                    if !key.starts_with("env:") && !key.starts_with("file:") {
                        return Err("signing.key must be an env: or file: reference".into());
                    }
                    result.signing_key = Some(if let Some(file) = key.strip_prefix("file:") {
                        format!("file:{}", root.join(file).display())
                    } else { key.to_owned() });
                }
            }
            "trim" => {
                result.trim_set = true;
                if value.as_bool() != Some(false)
                    && !value
                        .as_str()
                        .is_some_and(|text| matches!(text, "modules" | "members" | "aggressive"))
                {
                    return Err("trim must be false, modules, members or aggressive".into());
                }
                if let Some(level) = value.as_str() {
                    result.trim_modules = true;
                    result.trim_level = Some(level.to_owned());
                }
            }
            "compression" => {
                if value.as_bool() != Some(false)
                    && !value
                        .as_str()
                        .is_some_and(|text| matches!(text, "auto" | "lzh" | "lz4" | "zstd"))
                {
                    return Err("compression must be false, auto, lzh, lz4 or zstd".into());
                }
                match value.as_str() {
                    Some("lzh" | "auto") => result.compression = Some(true),
                    None => result.compression = Some(false),
                    _ => result.unsupported.push("compression".into()),
                }
            }
            "profileData" => result.profile_data = Some(root.join(string(value, &key)?)),
            "builtinCatalog" => result.builtin_catalog = Some(root.join(string(value, &key)?)),
            "runtimeCatalog" => result.runtime_catalog = Some(root.join(string(value, &key)?)),
            "snapshotAt" => result.snapshot_at = Some(string(value, &key)?.to_owned()),
            "stripLines" => {
                result.strip_lines = boolean(value, &key)?;
            }
            "keep" => result.keep = strings(value, &key)?,
            "assets" => result.assets = strings(value, &key)?,
            "locales" => {
                let locales = strings(value, &key)?;
                if locales.iter().any(|locale| !valid_locale(locale)) {
                    return Err("locales must contain nonempty BCP47 locale tags".into());
                }
                result.locales = (!locales.is_empty()).then_some(locales);
            }
            "targets" => {
                for (name, value) in value.as_object().ok_or("targets must be an object")? {
                    if name.is_empty() { return Err("target name must not be empty".into()); }
                    let mut target = NamedTarget::default();
                    for (key, value) in value.as_object().ok_or("named target must be an object")? {
                        let key = snake_to_camel(key);
                        let text = string(value, &key)?;
                        match key.as_str() {
                            "exe" => { target.executable = true; target.output = Some(root.join(text)); }
                            "out" => target.output = Some(root.join(text)),
                            "os" if matches!(text, "windows" | "linux" | "macos") => target.os = Some(text.into()),
                            "arch" if matches!(text, "x86_64" | "aarch64") => target.arch = Some(text.into()),
                            "mode" if matches!(text, "stub" | "link") => target.mode = Some(text.into()),
                            "subsystem" if matches!(text, "gui" | "console") => target.gui = text == "gui",
                            "device" => target.device = Some(text.into()),
                            "descriptor" => target.descriptor = Some(root.join(text)),
                            "icon" => target.icon = Some(root.join(text)),
                            "stub" => target.stub = Some(root.join(text)),
                            "runtimeLib" => target.runtime_lib = Some(root.join(text)),
                            _ => return Err(format!("invalid named target option {name}.{key}")),
                        }
                    }
                    if target.device.is_some() && target.descriptor.is_some() { return Err("named target cannot combine device and descriptor".into()); }
                    result.targets.insert(name.clone(), target);
                }
            }
            _ => {
                string(value, &key)?;
                result.unsupported.push(key);
            }
        }
    }
    Ok(result)
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("{key} must be a string"))
}

pub(crate) fn valid_locale(locale: &str) -> bool {
    let language = locale.split('-').next().unwrap_or_default();
    (2..=8).contains(&language.len()) && language.bytes().all(|byte| byte.is_ascii_alphabetic())
        && locale.split('-').all(|part| !part.is_empty() && part.len() <= 8 && part.bytes().all(|byte| byte.is_ascii_alphanumeric()))
}
fn boolean(value: &Value, key: &str) -> Result<bool, String> {
    value
        .as_bool()
        .ok_or_else(|| format!("{key} must be a boolean"))
}
fn strings(value: &Value, key: &str) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("{key} must be an array"))?
        .iter()
        .map(|item| string(item, key).map(str::to_owned))
        .collect()
}
fn snake_to_camel(key: &str) -> String {
    let mut upper = false;
    key.chars()
        .filter_map(|c| {
            if c == '_' {
                upper = true;
                None
            } else {
                let output = if upper { c.to_ascii_uppercase() } else { c };
                upper = false;
                Some(output)
            }
        })
        .collect()
}
fn distance(a: &str, b: &str) -> usize {
    let mut previous = (0..=b.len()).collect::<Vec<_>>();
    for (i, left) in a.bytes().enumerate() {
        let mut current = vec![i + 1; b.len() + 1];
        for (j, right) in b.bytes().enumerate() {
            current[j + 1] = (current[j] + 1)
                .min(previous[j + 1] + 1)
                .min(previous[j] + usize::from(left != right));
        }
        previous = current;
    }
    previous[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytecode_defaults_and_paths() {
        let value = serde_json::json!({
            "entry": "main.ts", "source_root": "src", "compression": false,
            "trim": false, "strip_lines": false, "assets": [], "keep": []
        });
        let config = parse(value.as_object().unwrap(), Path::new("project")).unwrap();
        assert_eq!(config.entry, Some(PathBuf::from("project/main.ts")));
        assert_eq!(config.source_root, Some(PathBuf::from("project/src")));
        assert_eq!(config.compression, Some(false));
        assert!(config.unsupported.is_empty());
    }

    #[test]
    fn rejects_unknown_keys_and_unsupported_codecs() {
        let typo = serde_json::json!({"stripLins": true});
        assert!(parse(typo.as_object().unwrap(), Path::new("."))
            .err().unwrap().contains("stripLines"));
        let codec = serde_json::json!({"compression": "zstd"});
        assert_eq!(parse(codec.as_object().unwrap(), Path::new("."))
            .unwrap().unsupported, ["compression"]);
    }

    #[test]
    fn named_targets_resolve_project_paths() {
        let value = serde_json::json!({"targets": {"win": {
            "exe": "dist/app.exe", "os": "windows", "arch": "x86_64",
            "mode": "stub", "subsystem": "gui", "icon": "assets/app.ico",
            "descriptor": "runtime.target", "stub": "runtime.exe"
        }}});
        let config = parse(value.as_object().unwrap(), Path::new("project")).unwrap();
        let target = &config.targets["win"];
        assert_eq!(target.output, Some(PathBuf::from("project/dist/app.exe")));
        assert_eq!(target.icon, Some(PathBuf::from("project/assets/app.ico")));
        assert!(target.executable && target.gui);
        assert!(config.unsupported.is_empty());
        let invalid = serde_json::json!({"targets": {"bad": {"os": "plan9"}}});
        assert!(parse(invalid.as_object().unwrap(), Path::new(".")).is_err());
    }
}
