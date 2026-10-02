use std::path::PathBuf;

/// Compute the native binding union for a dedicated runtime's installed app set.
pub fn runtime_imports(args: &[String]) -> Result<(), String> {
    if args.is_empty() { return Err("usage: lumen-cli runtime-imports APP.lmc ...".into()); }
    if args.iter().any(|arg| matches!(arg.as_str(), "--help" | "-h")) {
        println!("usage: lumen-cli runtime-imports APP.lmc ...");
        return Ok(());
    }
    let mut imports = std::collections::BTreeMap::new();
    let mut fingerprints = std::collections::BTreeMap::new();
    for path in args {
        let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let container = lumen_common::aot::NativeContainer::parse(&bytes)
            .map_err(|e| format!("{path}: {e}"))?;
        let language = match container.language {
            lumen_common::aot::Language::JavaScript => "js",
            lumen_common::aot::Language::Python => "py",
        };
        let fingerprint = (container.native_fp, container.lumen_version);
        if fingerprints.insert(language, fingerprint).is_some_and(|old| old != fingerprint) {
            return Err(format!("{path}: {language} app set has incompatible native fingerprints or versions"));
        }
        for import in container.required_imports {
            let key = (import.module.to_owned(), import.name.to_owned());
            if imports.insert(key.clone(), import.signature_hash).is_some_and(|old| old != import.signature_hash) {
                return Err(format!("incompatible native binding signatures: {}:{}", key.0, key.1));
            }
        }
    }
    let fingerprints = fingerprints.into_iter().map(|(language, (native_fp, version))| serde_json::json!({
        "language": language, "nativeFp": format!("{native_fp:016x}"),
        "lumenVersion": version.iter().map(|byte| format!("{byte:02x}")).collect::<String>(),
    })).collect::<Vec<_>>();
    let imports = imports.into_iter().map(|((module, name), signature)| serde_json::json!({
        "module": module, "name": name, "signatureHash": format!("{signature:016x}"),
    })).collect::<Vec<_>>();
    println!("{}", serde_json::to_string_pretty(&serde_json::json!({
        "format": 1, "fingerprints": fingerprints, "imports": imports,
    })).map_err(|e| e.to_string())?);
    Ok(())
}

pub fn symbolize_native(args: &[String]) -> Result<(), String> {
    let (blob_path, map_path, function, offset) = match args {
        [blob_path, function, offset] => (blob_path, None, function, offset),
        [blob_path, map_path, function, offset] => (blob_path, Some(map_path), function, offset),
        _ => {
            return Err(
                "usage: lumen-cli symbolize-native APP.lmc [APP.lmc.map] FUNCTION CODE_OFFSET"
                    .into(),
            );
        }
    };
    let function = function
        .parse::<u32>()
        .map_err(|_| "invalid function index")?;
    let offset = offset.parse::<u32>().map_err(|_| "invalid code offset")?;
    let blob = std::fs::read(blob_path).map_err(|e| format!("{blob_path}: {e}"))?;
    let container = lumen_common::aot::NativeContainer::parse(&blob)?;
    if container
        .functions
        .get(function as usize)
        .is_none_or(|entry| offset >= entry.len)
    {
        return Err("native code offset is outside the function".into());
    }
    let location = if let Some(map_path) = map_path {
        let map = std::fs::read(map_path).map_err(|e| format!("{map_path}: {e}"))?;
        let map = lumen_common::aot::sidecar::Sidecar::decode(&map)?;
        map.validate_for_blob(&blob)?;
        map.lookup(function, offset)
            .map(|(file, line, column)| (file.to_owned(), line, column))
    } else {
        let bytes = container
            .sections
            .iter()
            .find(|section| section.kind == lumen_common::aot::SEC_NATIVE_LINES)
            .ok_or("native blob has no embedded line table")?
            .data;
        let lines = lumen_common::aot::native_lines::decode(bytes, &container.functions)?;
        lines
            .lookup(function, offset)
            .map(|(file, line, column)| (file.to_owned(), line, column))
    };
    let (file, line, column) = location.ok_or("no source location for native code offset")?;
    println!("{file}:{line}:{column}");
    Ok(())
}

fn signing_seed(reference: &str) -> Result<[u8; 32], String> {
    let bytes = if let Some(name) = reference.strip_prefix("env:") {
        let value = std::env::var(name).map_err(|_| format!("signing environment variable {name:?} is unavailable"))?;
        if value.len() != 64 { return Err("signing environment value must be 64 hexadecimal digits".into()); }
        (0..32).map(|index| u8::from_str_radix(value.get(index * 2..index * 2 + 2)
            .ok_or("signing environment value must contain ASCII hexadecimal digits")?, 16)
            .map_err(|_| "signing environment value must contain hexadecimal digits".to_owned())).collect::<Result<Vec<_>, _>>()?
    } else {
        let path = reference.strip_prefix("file:").unwrap_or(reference);
        std::fs::read(path).map_err(|e| format!("signing key file {path}: {e}"))?
    };
    bytes.try_into().map_err(|_| "Ed25519 seed must be exactly 32 bytes".into())
}

pub fn sign_native(args: &[String]) -> Result<(), String> {
    let mut blob_path = None;
    let mut key_path = None;
    let mut output = None;
    let mut public_key_output = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--key" => key_path = Some(iter.next().ok_or("--key requires a file")?),
            "--output" | "-o" => output = Some(iter.next().ok_or("--output requires a file")?),
            "--public-key" => {
                public_key_output = Some(iter.next().ok_or("--public-key requires a file")?)
            }
            "--help" | "-h" => {
                println!(
                    "usage: lumen-cli sign-native APP.lmc --key ED25519_SEED32 --output APP.sig [--public-key PUB32]"
                );
                return Ok(());
            }
            _ if arg.starts_with('-') => return Err(format!("unknown sign-native option: {arg}")),
            _ if blob_path.is_none() => blob_path = Some(arg),
            _ => return Err(format!("unexpected sign-native argument: {arg}")),
        }
    }
    let blob_path = blob_path.ok_or("sign-native requires a native blob")?;
    let key_path = key_path.ok_or("sign-native requires --key")?;
    let output = output.ok_or("sign-native requires --output")?;
    if output == key_path || output == blob_path || public_key_output == Some(output) {
        return Err("sign-native outputs must differ from inputs and each other".into());
    }
    if public_key_output.is_some_and(|path| path == key_path || path == blob_path) {
        return Err("sign-native outputs must differ from inputs and each other".into());
    }
    if !key_path.starts_with("env:") {
        let key_file = PathBuf::from(key_path.strip_prefix("file:").unwrap_or(key_path))
            .canonicalize().map_err(|e| format!("signing key file: {e}"))?;
        if std::iter::once(output).chain(public_key_output).any(|path| PathBuf::from(path).canonicalize().is_ok_and(|path| path == key_file)) {
            return Err("sign-native output would overwrite the signing key".into());
        }
    }
    let blob = std::fs::read(blob_path).map_err(|e| format!("{blob_path}: {e}"))?;
    let seed = signing_seed(key_path)?;
    let signature = lumen_common::aot::signature::sign(&blob, &seed)?;
    if let Some(path) = public_key_output {
        let public_key = lumen_common::aot::signature::public_key(&seed);
        std::fs::write(path, public_key).map_err(|e| format!("{path}: {e}"))?;
    }
    std::fs::write(output, signature).map_err(|e| format!("{output}: {e}"))
}

pub fn pack_install(args: &[String]) -> Result<(), String> {
    let mut blob_path = None;
    let mut name = None;
    let mut sources = Vec::new();
    let mut entry = None;
    let mut scripts = Vec::new();
    let mut modules = Vec::new();
    let mut node_modules = false;
    let mut source_root = None;
    let mut output = None;
    let mut signature = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--name" => name = Some(iter.next().ok_or("--name requires an app name")?),
            "--source" => sources.push(iter.next().ok_or("--source requires a file")?),
            "--entry" => entry = Some(iter.next().ok_or("--entry requires a file")?),
            "--script" => scripts.push(iter.next().ok_or("--script requires a file")?),
            "--module" => modules.push(iter.next().ok_or("--module requires a file")?),
            "--node-modules" => node_modules = true,
            "--source-root" => {
                source_root = Some(iter.next().ok_or("--source-root requires a directory")?)
            }
            "--output" | "-o" => output = Some(iter.next().ok_or("--output requires a file")?),
            "--signature" => signature = Some(iter.next().ok_or("--signature requires a file")?),
            "--help" | "-h" => {
                println!(
                    "usage: lumen-cli pack-install APP.lmc --name NAME (--entry ENTRY [--script FILE] [--module FILE] [--node-modules] | --source SOURCE ...) [--source SOURCE ...] [--source-root DIR] --output APP.lumup [--signature SIG64]"
                );
                return Ok(());
            }
            _ if arg.starts_with('-') => return Err(format!("unknown pack-install option: {arg}")),
            _ if blob_path.is_none() => blob_path = Some(arg),
            _ => return Err(format!("unexpected pack-install argument: {arg}")),
        }
    }
    let blob_path = blob_path.ok_or("pack-install requires a native blob")?;
    let name = name.ok_or("pack-install requires --name")?;
    if entry.is_none() && scripts.is_empty() && modules.is_empty() && sources.is_empty() {
        return Err("pack-install requires --entry, --script, --module, or --source".into());
    }
    let output = output.ok_or("pack-install requires --output")?;
    if output == blob_path || sources.contains(&output) || signature == Some(output) {
        return Err("pack-install output must differ from its inputs".into());
    }
    let blob = std::fs::read(blob_path).map_err(|e| format!("{blob_path}: {e}"))?;
    let root = match source_root {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir().map_err(|e| e.to_string())?,
    }
    .canonicalize()
    .map_err(|e| format!("source root: {e}"))?;
    if !root.is_dir() {
        return Err("source root must be a directory".into());
    }
    let spec = lumen_aot::build::Spec {
        entry: entry.map(PathBuf::from),
        scripts: scripts.into_iter().map(PathBuf::from).collect(),
        modules: modules.into_iter().map(PathBuf::from).collect(),
        node_modules,
        walk: true,
        closed_world: true,
        ..Default::default()
    };
    let mut discovered =
        if spec.entry.is_some() || !spec.scripts.is_empty() || !spec.modules.is_empty() {
            lumen_aot::build::collect_inputs(&root, &spec)?
        } else {
            Vec::new()
        };
    discovered.extend(sources.into_iter().map(PathBuf::from));
    let output_existing = PathBuf::from(output).canonicalize().ok();
    if output_existing.as_ref().is_some_and(|out| {
        PathBuf::from(blob_path).canonicalize().ok().as_ref() == Some(out)
            || signature
                .is_some_and(|path| PathBuf::from(path).canonicalize().ok().as_ref() == Some(out))
    }) {
        return Err("pack-install output must differ from its inputs".into());
    }
    let mut source_set = Vec::with_capacity(discovered.len());
    let mut seen = std::collections::BTreeSet::new();
    for source in discovered {
        let path = source
            .canonicalize()
            .map_err(|e| format!("{}: {e}", source.display()))?;
        if !seen.insert(path.clone()) {
            continue;
        }
        if output_existing.as_ref() == Some(&path) {
            return Err("pack-install output must differ from its inputs".into());
        }
        let relative = path
            .strip_prefix(&root)
            .map_err(|_| format!("{}: source is outside --source-root", source.display()))?;
        let name = relative
            .components()
            .map(|component| {
                component
                    .as_os_str()
                    .to_str()
                    .ok_or_else(|| format!("{}: source path is not UTF-8", source.display()))
            })
            .collect::<Result<Vec<_>, _>>()?
            .join("/");
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", source.display()))?;
        source_set.push((name, bytes));
    }
    let source_refs = source_set
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect::<Vec<_>>();
    let source_hash = lumen_common::aot::sidecar::hash_sources(&source_refs)?;
    let payload = lumen_common::aot::install::encode_install_payload(name, source_hash, &blob)?;
    let signature_bytes = signature
        .map(|path| std::fs::read(path).map_err(|e| format!("{path}: {e}")))
        .transpose()?;
    let signature: Option<[u8; 64]> = signature_bytes
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| "signature must be exactly 64 bytes")
        })
        .transpose()?;
    let frame = lumen_common::aot::install::encode(
        lumen_common::aot::install::Kind::Install,
        &payload,
        signature.as_ref(),
    )?;
    std::fs::write(output, frame).map_err(|e| format!("{output}: {e}"))
}

pub fn write_target(args: &[String]) -> Result<(), String> {
    lumen_runtime::Runtime::freeze_native_catalog();
    let [out] = args else {
        return Err("usage: lumen-cli target TARGET_FILE".into());
    };
    let bytes = lumen::target::host().encode()?;
    std::fs::write(out, bytes).map_err(|e| format!("{out}: {e}"))
}

pub fn compile(args: &[String]) -> Result<(), String> {
    let expanded = args.iter().flat_map(|arg| match arg.strip_prefix("--").and_then(|value| value.split_once('=')) {
        Some((name, value)) => vec![format!("--{name}"), value.to_owned()],
        None => vec![arg.clone()],
    }).collect::<Vec<_>>();
    let args = expanded.as_slice();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("usage: lumen-cli compile [ENTRY] [--config FILE] [--tier bc|mc] [-o OUTPUT] [--target NAME|TARGET_FILE|@PORT] [--os linux|windows|macos] [--profile full|nojit|aot] [--exe] [--mode stub|link] [--stub FILE|--runtime-lib FILE] [--trim false|modules|members|aggressive] [--keep PATTERN]");
        println!("runtime: --runtime-catalog FILE --locales en,nl --asset GLOB --linker PROGRAM --link-arg ARG --icon APP.ico --windows-subsystem console|gui");
        println!("native: --profile-data FILE --snapshot-at FUNCTION --builtin-catalog FILE --signing-key env:NAME|file:PATH --strip-lines");
        println!("sources: --script --module FILE --node-modules --source-root DIR --keep-source --compression false|auto|lzh; default trim: mc=members, bc=modules");
        return Ok(());
    }
    let mut config_file = None;
    lumen_runtime::Runtime::freeze_native_catalog();
    for (index, arg) in args.iter().enumerate() {
        if arg == "--config" {
            config_file = Some(
                args.get(index + 1)
                    .ok_or("--config requires a JSON or TOML file")?
                    .as_str(),
            );
        }
    }
    let mut config = crate::aot_config::load(config_file)?;
    let mut host_inputs = config.file.iter().cloned().collect::<Vec<_>>();
    let trim_set = config.trim_set || args.iter().any(|arg| arg == "--trim");
    if args.iter().any(|arg| arg == "--compression") {
        config.unsupported.retain(|key| key != "compression");
    }
    if args.iter().any(|arg| arg == "--trim") {
        config.unsupported.retain(|key| key != "trim");
    }
    if !config.unsupported.is_empty() {
        return Err(format!(
            "project configuration uses unimplemented AOT option(s): {}",
            config.unsupported.join(", ")
        ));
    }
    let selected_name = args.windows(2).rev().find(|pair| pair[0] == "--target").map(|pair| pair[1].as_str());
    let named = selected_name.and_then(|name| config.targets.remove(name));
    let named_arch = named.as_ref().and_then(|target| target.arch.clone());
    let mut locales = config.locales;
    let mut runtime_catalog = config.runtime_catalog.or_else(|| std::env::var_os("LUMEN_RUNTIME_CATALOG").map(PathBuf::from));
    let mut entry = config.entry;
    let mut output = config.output;
    let mut tier = config.tier.unwrap_or_else(|| "bc".into());
    let mut strip_lines = config.strip_lines;
    let mut profile = config.profile;
    let mut target = named.as_ref().and_then(|target| target.device.as_ref().map(|device| format!("@{device}"))
        .or_else(|| target.descriptor.as_ref().map(|path| path.to_string_lossy().into_owned())));
    let mut target_os = named.as_ref().and_then(|target| target.os.clone()).unwrap_or_else(|| std::env::consts::OS.to_owned());
    let mut executable = named.as_ref().is_some_and(|target| target.executable);
    let mut stub = named.as_ref().and_then(|target| target.stub.clone());
    let mut windows_gui = named.as_ref().is_some_and(|target| target.gui);
    let mut icon = named.as_ref().and_then(|target| target.icon.clone());
    if let Some(path) = named.as_ref().and_then(|target| target.output.clone()) { output = Some(path); }
    let mut profile_data = config.profile_data;
    let mut snapshot_at = config.snapshot_at;
    let mut signing_key = config.signing_key;
    let mut builtin_catalog = config.builtin_catalog;
    let mut mode = named.as_ref().and_then(|target| target.mode.clone()).unwrap_or_else(|| "stub".to_owned());
    let mut runtime_lib = named.as_ref().and_then(|target| target.runtime_lib.clone());
    let mut linker = None;
    let mut link_args = Vec::new();
    let mut script = config.script;
    let mut spec = lumen_aot::build::Spec {
        assets: config.assets,
        asset_root: config.project_root,
        walk: true,
        node_modules: config.node_modules,
        keep_source: config.keep_source,
        modules: config.modules,
        root: config.source_root,
        uncompressed_source: config.compression == Some(false),
        trim_modules: config.trim_modules,
        trim_level: config.trim_level,
        keep: config.keep,
        ..Default::default()
    };
    let mut iter = args.iter();
    let mut positional_seen = false;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--exe" => executable = true,
            "--mode" => {
                mode = iter.next().ok_or("--mode requires stub or link")?.clone();
                if !matches!(mode.as_str(), "stub" | "link") { return Err("--mode requires stub or link".into()); }
                executable = true;
            }
            "--runtime-lib" => runtime_lib = Some(PathBuf::from(iter.next().ok_or("--runtime-lib requires a static library")?)),
            "--runtime-catalog" => runtime_catalog = Some(PathBuf::from(iter.next().ok_or("--runtime-catalog requires a JSON catalog")?)),
            "--linker" => linker = Some(iter.next().ok_or("--linker requires a linker program")?.clone()),
            "--link-arg" => link_args.push(iter.next().ok_or("--link-arg requires one argument")?.clone()),
            "--icon" => icon = Some(PathBuf::from(iter.next().ok_or("--icon requires an ICO file")?)),
            "--asset" => spec.assets.push(iter.next().ok_or("--asset requires a source-root-relative glob")?.clone()),
            "--locales" => {
                let value = iter.next().ok_or("--locales requires comma-separated locale tags")?;
                if value.split(',').any(|locale| !crate::aot_config::valid_locale(locale)) { return Err("--locales requires BCP47 locale tags".into()); }
                locales = Some(value.split(',').map(str::to_owned).collect());
            }
            "--profile-data" => profile_data = Some(PathBuf::from(iter.next().ok_or("--profile-data requires a feedback file")?)),
            "--snapshot-at" => snapshot_at = Some(iter.next().ok_or("--snapshot-at requires an entry function")?.clone()),
            "--builtin-catalog" => builtin_catalog = Some(PathBuf::from(iter.next().ok_or("--builtin-catalog requires a JSON manifest")?)),
            "--signing-key" => {
                let reference = iter.next().ok_or("--signing-key requires env:NAME or file:PATH")?;
                if !reference.starts_with("env:") && !reference.starts_with("file:") {
                    return Err("--signing-key requires env:NAME or file:PATH".into());
                }
                signing_key = Some(reference.clone());
            }
            "--stub" => stub = Some(PathBuf::from(iter.next().ok_or("--stub requires an executable")?)),
            "--windows-subsystem" => windows_gui = match iter.next().map(String::as_str) {
                Some("console") => false,
                Some("gui") => true,
                _ => return Err("--windows-subsystem requires console or gui".into()),
            },
            "--tier" => tier = iter.next().ok_or("--tier requires bc or mc")?.clone(),
            "--profile" => {
                let name = iter.next().ok_or("--profile requires full, nojit or aot")?;
                if !matches!(name.as_str(), "full" | "nojit" | "aot") {
                    profile_data = Some(PathBuf::from(name));
                    continue;
                }
                profile = Some(name.clone());
            }
            "--config" => {
                iter.next().ok_or("--config requires a JSON or TOML file")?;
            }
            "-o" | "--output" => {
                output = Some(PathBuf::from(
                    iter.next().ok_or("--output requires a path")?,
                ))
            }
            "--target" => {
                let value = iter.next().ok_or("--target requires a named target, target file or @PORT")?;
                if named.is_none() { target = Some(value.clone()); }
            }
            "--os" => {
                target_os = iter.next().ok_or("--os requires linux, windows or macos")?.clone();
                if !matches!(target_os.as_str(), "linux" | "windows" | "macos") { return Err("--os requires linux, windows or macos".into()); }
            }
            "--script" => script = true,
            "--node-modules" => spec.node_modules = true,
            "--module" => spec.modules.push(PathBuf::from(iter.next().ok_or("--module requires a file")?)),
            "--source-root" => spec.root = Some(PathBuf::from(iter.next().ok_or("--source-root requires a directory")?)),
            "--compression" => {
                spec.uncompressed_source = match iter.next().map(String::as_str) {
                    Some("false" | "none") => true,
                    Some("auto" | "lzh") => false,
                    _ => return Err("AOT-BC compression must be false, auto or lzh".into()),
                };
            }
            "--trim" => {
                let level = iter.next().ok_or("--trim requires false, modules, members or aggressive")?;
                if !matches!(level.as_str(), "false" | "none" | "modules" | "members" | "aggressive") { return Err("--trim requires false, modules, members or aggressive".into()); }
                spec.trim_modules = !matches!(level.as_str(), "false" | "none");
                spec.trim_level = spec.trim_modules.then(|| level.clone());
            }
            "--keep" => spec.keep.push(iter.next().ok_or("--keep requires a symbol pattern")?.clone()),
            "--strip-lines" => strip_lines = true,
            "--keep-source" => spec.keep_source = vec!["**".into()],
            "--help" | "-h" => {
                println!(
                    "usage: lumen-cli compile [ENTRY] [--config FILE] [--tier bc|mc] [-o OUTPUT] [--target NAME|TARGET_FILE|@PORT] [--os linux|windows|macos] [--profile full|nojit|aot] [--profile-data FILE] [--snapshot-at FUNCTION] [--builtin-catalog FILE] [--signing-key env:NAME|file:PATH] [--exe] [--mode stub|link] [--stub FILE|--runtime-lib FILE] [--linker PROGRAM] [--link-arg ARG] [--icon APP.ico] [--windows-subsystem console|gui] [--script] [--module FILE] [--node-modules] [--keep-source] [--source-root DIR] [--compression false|auto|lzh] [--trim false|modules|members|aggressive] [--keep PATTERN]"
                );
                return Ok(());
            }
            _ if arg.starts_with('-') => return Err(format!("unknown compile option: {arg}")),
            _ if !positional_seen => {
                entry = Some(PathBuf::from(arg));
                positional_seen = true;
            }
            _ => return Err(format!("unexpected compile argument: {arg}")),
        }
    }
    if !matches!(tier.as_str(), "bc" | "mc") {
        return Err(format!("unknown AOT tier: {tier}"));
    }
    if !trim_set {
        spec.trim_modules = true;
        spec.trim_level = Some(if tier == "mc" { "members" } else { "modules" }.into());
    }
    if tier == "bc" && profile.as_deref() == Some("aot") {
        return Err("Aot profile requires a native-only blob".into());
    }
    if tier == "bc" && strip_lines {
        return Err("--strip-lines requires the native tier".into());
    }
    if tier == "bc" && profile_data.is_some() {
        return Err("feedback profiles require --tier mc".into());
    }
    if tier == "bc" && snapshot_at.is_some() { return Err("initialized heap snapshots require --tier mc".into()); }
    if tier == "bc" && signing_key.is_some() { return Err("native signing requires --tier mc".into()); }
    if tier == "bc" && spec.trim_level.as_deref().is_some_and(|level| matches!(level, "members" | "aggressive")) {
        return Err("member/aggressive trimming requires --tier mc; AOT-BC supports modules".into());
    }
    let feedback = profile_data.map(|path| {
        host_inputs.push(path.clone());
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        lumen::feedback::Profile::decode(&bytes)
    }).transpose()?;
    spec.native_catalog = builtin_catalog.map(|path| {
        host_inputs.push(path.clone());
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let object = value.as_object().ok_or("builtin catalog must be an object")?;
        if object.keys().any(|key| !matches!(key.as_str(), "format" | "imports" | "fingerprints"))
            || object.get("format").and_then(serde_json::Value::as_u64) != Some(1) { return Err("builtin catalog requires format 1 and imports".into()); }
        let imports = object.get("imports").and_then(serde_json::Value::as_array).ok_or("builtin catalog imports must be an array")?;
        let mut catalog = Vec::new();
        for import in imports {
            let import = import.as_object().ok_or("builtin catalog import must be an object")?;
            if import.len() != 3 || import.keys().any(|key| !matches!(key.as_str(), "module" | "name" | "signatureHash")) {
                return Err("builtin catalog imports require module, name and signatureHash".into());
            }
            let module = import["module"].as_str().filter(|name| !name.is_empty() && !name.contains('\0')).ok_or("builtin catalog module must be a nonempty string")?;
            let name = import["name"].as_str().filter(|name| !name.contains('\0')).ok_or("builtin catalog name must be a string")?;
            let signature = import["signatureHash"].as_str().filter(|value| value.len() == 16).ok_or("builtin catalog signatureHash must have 16 hexadecimal digits")?;
            let signature = u64::from_str_radix(signature, 16).map_err(|_| "invalid builtin catalog signatureHash")?;
            if signature == 0 { return Err("builtin catalog signatures must be versioned and nonzero".into()); }
            let entry = (module.to_owned(), name.to_owned(), signature);
            if catalog.last().is_some_and(|last: &(String, String, u64)| (&last.0, &last.1) >= (&entry.0, &entry.1)) {
                return Err("builtin catalog imports must be unique and sorted by module/name".into());
            }
            catalog.push(entry);
        }
        Ok::<_, String>(catalog)
    }).transpose()?;
    let entry = entry.ok_or("compile requires an entry file")?;
    if entry.extension().is_some_and(|e| e == "py") {
        return Err("Python AOT compilation is not linked".into());
    }
    if runtime_catalog.is_none() && executable {
        let path = std::env::current_exe().map_err(|error| error.to_string())?.with_file_name("runtime-catalog.json");
        if path.is_file() { runtime_catalog = Some(path); }
    }
    let mut selected_runtime_target = None;
    if executable && ((mode == "link" && runtime_lib.is_none()) || (mode == "stub" && stub.is_none())) {
        if let Some(catalog) = &runtime_catalog {
            host_inputs.push(catalog.clone());
            let mut roots = spec.clone();
            if script { roots.scripts.push(entry.clone()); } else { roots.entry = Some(entry.clone()); }
            let preflight = lumen_aot::build::compile(std::env::current_dir().map_err(|error| error.to_string())?, &roots)?;
            let profile_name = profile.as_deref().unwrap_or("full");
            let required = crate::aot_runtime_plan::features(&preflight.inputs, &preflight.required_modules, profile_name, spec.trim_modules, locales.is_some());
            let variant = crate::aot_runtime_plan::select(catalog, &target_os, named_arch.as_deref().unwrap_or(std::env::consts::ARCH),
                profile_name, &mode, &required, locales.as_deref())?;
            let bytes = std::fs::read(&variant.descriptor).map_err(|error| format!("{}: {error}", variant.descriptor.display()))?;
            selected_runtime_target = Some(lumen::target::TargetSpec::decode(&bytes)?);
            if target.is_none() { target = Some(variant.descriptor.to_string_lossy().into_owned()); }
            if stub.is_none() { stub = variant.stub; }
            if runtime_lib.is_none() && mode == "link" { runtime_lib = variant.library; }
        }
    }
    let mut native_target = lumen::target::host();
    if let Some(path) = target {
        let spec = if let Some(device) = path.strip_prefix('@') {
            crate::aot_transport::target_from_reference(device)?
        } else {
            host_inputs.push(PathBuf::from(&path));
            let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            lumen::target::TargetSpec::decode(&bytes)?
        };
        let host = lumen::target::host();
        if profile.as_deref().is_some_and(|name| {
            (name == "full" && spec.profile != lumen::target::Profile::Full)
                || (name == "nojit" && spec.profile != lumen::target::Profile::NoJit)
                || (name == "aot" && spec.profile != lumen::target::Profile::Aot)
        }) {
            return Err("target profile differs from project configuration".into());
        }
        if tier == "bc" && (spec.profile == lumen::target::Profile::Aot
            || spec.bytecode_fp != host.bytecode_fp
            || spec.lumen_version != host.lumen_version) {
            return Err("target cannot load this engine's AOT-BC blobs".into());
        }
        native_target = spec;
    }
    if selected_runtime_target.is_some_and(|selected| selected != native_target) { return Err("runtime catalog descriptor differs from requested target".into()); }
    if executable {
        let expected = match (target_os.as_str(), native_target.arch) {
            ("windows", _) => lumen::target::Abi::Win64,
            ("macos", lumen::target::Arch::Aarch64) => lumen::target::Abi::Apple64,
            ("linux", lumen::target::Arch::Aarch64) => lumen::target::Abi::Aapcs64,
            _ => lumen::target::Abi::SysV64,
        };
        if native_target.abi != expected { return Err("standalone OS requires a matching target ABI descriptor; pass --target".into()); }
    }
    if let Some(arch) = named_arch {
        let arch = match arch.as_str() { "x86_64" => lumen::target::Arch::X86_64, _ => lumen::target::Arch::Aarch64 };
        if native_target.arch != arch { return Err("named target architecture requires a matching descriptor or device target".into()); }
    }
    if tier == "mc" && profile.as_deref().is_some_and(|name| match name {
        "full" => native_target.profile != lumen::target::Profile::Full,
        "nojit" => native_target.profile != lumen::target::Profile::NoJit,
        "aot" => native_target.profile != lumen::target::Profile::Aot,
        _ => false,
    }) { return Err("requested runtime profile requires --target from that runtime".into()); }
    if !executable && (stub.is_some() || windows_gui || icon.is_some()) {
        return Err("--stub, --icon and --windows-subsystem require --exe".into());
    }
    if executable && profile.as_deref().is_some_and(|p| p != "full") && stub.is_none() {
        if mode != "link" || runtime_lib.is_none() { return Err("nojit/aot standalone profiles require an explicitly built --stub or --runtime-lib".into()); }
    }
    if mode == "link" {
        if stub.is_some() || icon.is_some() { return Err("link mode uses --runtime-lib and does not accept --stub/--icon".into()); }
        if runtime_lib.is_none() { return Err("link mode requires --runtime-lib; build lumen-exe-runtime for the target first".into()); }
    } else if runtime_lib.is_some() || linker.is_some() || !link_args.is_empty() { return Err("--runtime-lib/--linker/--link-arg require --mode link".into()); }
    let stub = if executable && mode == "stub" { Some(stub.unwrap_or(std::env::current_exe().map_err(|e| e.to_string())?)) } else { None };
    host_inputs.extend(stub.iter().chain(icon.iter()).chain(runtime_lib.iter()).cloned());
    spec.extra_inputs.extend(host_inputs);
    let out = output.unwrap_or_else(|| entry.with_extension(if executable { if cfg!(windows) { "exe" } else { "" } } else if tier == "mc" { "lmc" } else { "lbc" }));
    let initializer = entry.clone();
    if script {
        spec.scripts.push(entry);
    } else {
        spec.entry = Some(entry);
    }
    let base = std::env::current_dir().map_err(|e| e.to_string())?;
    let len = if tier == "mc" {
        if !lumen::native_aot::compiler_ready() {
            return Err("AOT-MC publishing is disabled until native runtime semantics are complete".into());
        }
        if native_target.native_fp == 0 {
            return Err("target does not advertise a native ABI".into());
        }
        let mut build = lumen_aot::native::compile_with_options(&base, &spec, &native_target, feedback.as_ref(), snapshot_at.as_deref())?;
        if snapshot_at.is_some() {
            let mut runtime = lumen_runtime::Runtime::new();
            runtime.install_native_builtins()?;
            let baseline = lumen::precompiled::NativeSnapshotBaseline::new(runtime.engine());
            let initializer = initializer.to_str().ok_or("snapshot initializer path must be UTF-8")?;
            let cjs = build.snapshot_units.iter().any(|(_, unit, _)| unit.kind() == lumen::SourceKind::CommonJs);
            if cjs {
                runtime.engine().prepare_cjs_snapshot(&build.snapshot_units)?;
                runtime.engine().install_snapshot_cjs_require_router()?;
            }
            if script {
                let source = std::fs::read_to_string(initializer).map_err(|e| format!("{initializer}: {e}"))?;
                let source = source.trim_start_matches('\u{feff}');
                let stripped;
                let source = if std::path::Path::new(initializer).extension().is_some_and(|extension| matches!(extension.to_str(), Some("ts" | "mts" | "cts" | "tsx"))) {
                    stripped = lumen::typescript::strip_types(source).map_err(|error| error.to_string())?;
                    stripped.as_str()
                } else { source };
                match runtime.engine().eval(&source, false) {
                    Ok(lumen::Completion::Throw { name, message }) => return Err(format!("snapshot initialization: {name}: {message}")),
                    Err(error) => return Err(format!("snapshot initialization: {}", error.message)),
                    _ => {}
                }
            } else { runtime.run_module(initializer)?; }
            if cjs { runtime.engine().finish_cjs_snapshot_initialization(); }
            let status = runtime.run_until_idle();
            if !status.idle || status.halted {
                return Err("snapshot initialization must finish without pending host tasks, timers or process termination".into());
            }
            let snapshot = build.snapshot_functions.capture(runtime.engine(), &baseline)?;
            build.attach_snapshot(&snapshot)?;
        }
        let map_path = PathBuf::from(format!("{}.map", out.display()));
        let signature_path = PathBuf::from(format!("{}.sig", out.display()));
        let trim_path = out.with_extension("trim.txt");
        let runtime_plan_path = PathBuf::from(format!("{}.runtime.json", out.display()));
        if spec.trim_modules && trim_path == out { return Err("output and trim report paths must differ".into()); }
        let seed = signing_key.as_deref().map(signing_seed).transpose()?;
        for output in [&out, &runtime_plan_path].into_iter().chain(strip_lines.then_some(&map_path)).chain(seed.is_some().then_some(&signature_path)).chain(spec.trim_modules.then_some(&trim_path)) {
        if let Ok(path) = output.canonicalize() {
            if build.inputs.iter().any(|input| input.canonicalize().is_ok_and(|input| input == path)) {
                return Err("output would overwrite a bundled source".into());
            }
        }
        }
        if let Some(key) = signing_key.as_deref().and_then(|key| key.strip_prefix("file:")) {
            let key = PathBuf::from(key).canonicalize().map_err(|e| e.to_string())?;
            if [&out, &map_path, &signature_path, &trim_path, &runtime_plan_path].iter().any(|path| path.canonicalize().is_ok_and(|path| path == key)) {
                return Err("compile output would overwrite the signing key".into());
            }
        }
        for warning in &build.warnings { eprintln!("warning: {warning}"); }
        crate::aot_runtime_plan::write(&out, &build.inputs, &build.required_modules,
            match native_target.profile { lumen::target::Profile::Aot => "aot", lumen::target::Profile::NoJit => "nojit", _ => "full" },
            spec.trim_modules, locales.as_deref(), &target_os)?;
        let blob = if strip_lines {
            let (blob, map) = build.encode_stripped(&native_target)?;
            std::fs::write(&map_path, map).map_err(|e| format!("{}: {e}", map_path.display()))?;
            blob
        } else { build.encode(&native_target)? };
        if spec.trim_modules {
            build.trim_report.push(format!("output bytes: {}", blob.len()));
            for module in &build.required_modules { build.trim_report.push(format!("required native: {module}")); }
            for warning in &build.warnings { build.trim_report.push(format!("warning: {warning}")); }
            std::fs::write(&trim_path, format!("{}\n", build.trim_report.join("\n"))).map_err(|e| format!("{}: {e}", trim_path.display()))?;
        }
        if let Some(seed) = seed {
            let signature = lumen_common::aot::signature::sign(&blob, &seed)?;
            std::fs::write(&signature_path, signature).map_err(|e| format!("{}: {e}", signature_path.display()))?;
        }
        if mode == "link" {
            link_executable(&blob, &native_target, Some(&build.image), &target_os, &out, runtime_lib.as_ref().unwrap(), linker.as_deref(), &link_args, windows_gui, &build.inputs)?
        } else if let Some(stub) = &stub {
            lumen_os::embedded::package(stub, &out, &blob, native_target.arch, windows_gui, icon.as_deref())?
        } else {
            std::fs::write(&out, &blob).map_err(|e| format!("{}: {e}", out.display()))?;
            blob.len()
        }
    } else if executable {
        let bundle = lumen_aot::build::compile(&base, &spec)?;
        if let Ok(output) = out.canonicalize() {
            if bundle.inputs.iter().any(|input| input.canonicalize().is_ok_and(|input| input == output)) {
                return Err("output would overwrite a bundled source".into());
            }
        }
        for warning in &bundle.warnings { eprintln!("warning: {warning}"); }
        crate::aot_runtime_plan::write(&out, &bundle.inputs, &bundle.required_modules, profile.as_deref().unwrap_or("full"),
            spec.trim_modules, locales.as_deref(), &target_os)?;
        if mode == "link" {
            link_executable(&bundle.blob, &native_target, None, &target_os, &out, runtime_lib.as_ref().unwrap(), linker.as_deref(), &link_args, windows_gui, &bundle.inputs)?
        } else { lumen_os::embedded::package(stub.as_ref().unwrap(), &out, &bundle.blob, native_target.arch, windows_gui, icon.as_deref())? }
    } else {
        if locales.is_some() { return Err("--locales configures a standalone runtime build; use --exe".into()); }
        lumen_aot::build::compile_to(
        base,
        &spec,
        &out,
    )? };
    println!("{} ({} bytes, AOT-{})", out.display(), len, tier.to_uppercase());
    Ok(())
}

fn link_executable(blob: &[u8], target: &lumen::target::TargetSpec, image: Option<&lumen_codegen::aot_image::Image>, target_os: &str, out: &std::path::Path,
    runtime: &std::path::Path, linker: Option<&str>, args: &[String], gui: bool, inputs: &[PathBuf]) -> Result<usize, String> {
    use lumen_codegen::aot_image::{standalone_object, ObjectFormat};
    let format = match target_os {
        "windows" => ObjectFormat::Coff,
        "macos" => ObjectFormat::MachO,
        "linux" => ObjectFormat::Elf,
        _ => return Err("standalone link mode supports linux, windows and macos".into()),
    };
    let windows = matches!(format, ObjectFormat::Coff);
    if gui && !windows { return Err("--windows-subsystem requires a Windows link target".into()); }
    let object = out.with_extension(format!("link-stage-{}.{}", std::process::id(), if windows { "obj" } else { "o" }));
    if object == out { return Err("linked output and object paths must differ".into()); }
    let runtime_path = runtime.canonicalize().map_err(|e| format!("{}: {e}", runtime.display()))?;
    {
        use std::io::Read;
        let mut magic = [0u8; 8];
        std::fs::File::open(&runtime_path).and_then(|mut file| file.read_exact(&mut magic)).map_err(|error| format!("runtime static archive: {error}"))?;
        if &magic != b"!<arch>\n" { return Err("--runtime-lib must be a static archive, not a shared library or executable".into()); }
    }
    for path in [&object, out] {
        if let Ok(path) = path.canonicalize() {
            if path == runtime_path || inputs.iter().any(|input| input.canonicalize().is_ok_and(|input| input == path)) {
                return Err("link output would overwrite the runtime library or a bundled source".into());
            }
        }
    }
    if let Some(parent) = out.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let bytes = match image {
        Some(image) => image.standalone_link_object(blob, target, format)?,
        None => standalone_object(blob, target, format)?,
    };
    let stage = out.with_extension(format!("link-stage-{}", std::process::id()));
    std::fs::OpenOptions::new().write(true).create_new(true).open(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let mut object_created = false;
    let result = (|| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&object).map_err(|e| format!("{}: {e}", object.display()))?;
        object_created = true;
        file.write_all(&bytes).map_err(|e| format!("{}: {e}", object.display()))?;
        drop(file);
        let mut command = native_linker(linker, target_os)?;
        if windows {
            command.args(["/NOLOGO", "/OPT:REF", "/OPT:ICF", "/Brepro"]).arg(format!("/OUT:{}", stage.display()))
                .arg(if gui { "/SUBSYSTEM:WINDOWS" } else { "/SUBSYSTEM:CONSOLE" })
                .arg(if target.arch == lumen::target::Arch::Aarch64 { "/MACHINE:ARM64" } else { "/MACHINE:X64" })
                .arg("/ENTRY:mainCRTStartup").arg(&object).arg(&runtime_path)
                .args(["msvcrt.lib", "ucrt.lib", "vcruntime.lib", "kernel32.lib", "advapi32.lib", "ws2_32.lib", "userenv.lib", "bcrypt.lib", "ntdll.lib"]);
        } else {
            if target.arch != lumen::target::host().arch && command.get_program().to_string_lossy().contains("clang") {
                command.arg(match (target_os, target.arch) {
                    ("macos", lumen::target::Arch::Aarch64) => "--target=arm64-apple-darwin",
                    ("macos", _) => "--target=x86_64-apple-darwin",
                    ("linux", lumen::target::Arch::Aarch64) => "--target=aarch64-unknown-linux-gnu",
                    _ => "--target=x86_64-unknown-linux-gnu",
                });
            }
            command.arg(&object).arg(&runtime_path).arg("-o").arg(&stage);
            if matches!(format, ObjectFormat::Elf) { command.args(["-Wl,--gc-sections,--build-id=none", "-ldl", "-lpthread", "-lm", "-lrt", "-lutil"]); }
            else { command.arg("-Wl,-dead_strip,-no_uuid"); }
        }
        let output = command.args(args).output().map_err(|e| format!("native linker: {e}"))?;
        if !output.status.success() { return Err(format!("native linker failed ({}): {}{}", output.status, String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr))); }
        let bytes = std::fs::read(&stage).map_err(|e| e.to_string())?;
        if lumen_common::executable::architecture(&bytes)? != target.arch { return Err("native linker produced a different target architecture".into()); }
        let size = bytes.len();
        lumen_os::embedded::publish(&stage, out)?;
        Ok(size)
    })();
    if object_created { let _ = std::fs::remove_file(&object); }
    if result.is_err() { let _ = std::fs::remove_file(&stage); }
    result
}

fn native_linker(explicit: Option<&str>, os: &str) -> Result<std::process::Command, String> {
    if let Some(program) = explicit.map(std::ffi::OsString::from).or_else(|| std::env::var_os("LUMEN_LINKER")) {
        let name = std::path::Path::new(&program).file_stem().and_then(|name| name.to_str()).unwrap_or_default();
        if os != "windows" && matches!(name, "rust-lld" | "ld.lld" | "ld64.lld") {
            return Err("Unix --linker must be a compiler driver (cc/clang), which supplies target CRT and SDK search paths; select LLD through --link-arg=-fuse-ld=PATH".into());
        }
        let rust_lld = name == "rust-lld";
        let mut command = std::process::Command::new(program);
        if os == "windows" && rust_lld { command.args(["-flavor", "link"]); }
        return Ok(command);
    }
    if os != std::env::consts::OS { return Err("cross-OS linking requires --linker and target SDK arguments via --link-arg".into()); }
    let executable = if cfg!(windows) { "rust-lld.exe" } else { "rust-lld" };
    let mut bundled = std::env::current_exe().ok().and_then(|path| path.parent().map(|path| path.join(executable))).filter(|path| path.is_file());
    if bundled.is_none() {
        let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
        if let Ok(result) = std::process::Command::new(rustc).args(["--print", "sysroot"]).output() {
            if result.status.success() {
                let sysroot = PathBuf::from(String::from_utf8_lossy(&result.stdout).trim());
                if let Ok(entries) = std::fs::read_dir(sysroot.join("lib/rustlib")) {
                    let mut paths = entries.filter_map(Result::ok).map(|entry| entry.path().join("bin").join(executable))
                        .filter(|path| path.is_file()).collect::<Vec<_>>();
                    paths.sort();
                    bundled = paths.into_iter().next();
                }
            }
        }
    }
    if os == "windows" {
        if let Some(program) = bundled {
            let mut command = std::process::Command::new(program);
            command.args(["-flavor", "link"]);
            return Ok(command);
        }
        return Ok(std::process::Command::new("link"));
    }
    if let Some(program) = std::env::var_os("CC") { return Ok(std::process::Command::new(program)); }
    let clang = std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|directory| directory.join("clang").is_file()));
    if let Some(program) = bundled.filter(|_| clang) {
        // Clang supplies CRT/SDK search paths while the bundled linker handles sections.
        let mut command = std::process::Command::new("clang");
        command.arg(format!("-fuse-ld={}", program.display()));
        command.arg(if os == "macos" { "-Wl,-flavor,darwin" } else { "-Wl,-flavor,gnu" });
        return Ok(command);
    }
    Ok(std::process::Command::new("cc"))
}

pub fn record_profile(args: &[String]) -> Result<(), String> {
    let mut output = None;
    let mut entry = None;
    let mut script_args = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if matches!(arg.as_str(), "--record-profile" | "-o" | "--output") {
            output = Some(PathBuf::from(iter.next().ok_or("profile output requires a path")?));
        } else if arg == "--" {
            script_args.extend(iter.cloned());
            break;
        } else if entry.is_none() {
            entry = Some(arg.clone());
        } else { script_args.push(arg.clone()); }
    }
    let entry = entry.ok_or("usage: lumen-cli record-profile -o APP.prof ENTRY [ARGS...] or run --record-profile APP.prof ENTRY [ARGS...]")?;
    let output = output.ok_or("profile recording requires an output path")?;
    if output.canonicalize().is_ok_and(|path| PathBuf::from(&entry).canonicalize().is_ok_and(|entry| entry == path)) {
        return Err("profile output would overwrite the entry source".into());
    }
    let mut runtime = lumen_runtime::Runtime::new();
    let argv0 = std::env::args().next().unwrap_or_default();
    script_args.insert(0, entry.clone());
    runtime.set_process_args(&argv0, &[], &script_args);
    let (result, profile) = lumen::feedback::record(|| {
        let result = if crate::is_esm_entry(&entry) { runtime.run_module(&entry) } else { runtime.run_main(&entry) };
        if result.is_ok() { runtime.run_to_completion(); }
        result
    });
    result?;
    std::fs::write(&output, profile.encode()).map_err(|e| format!("{}: {e}", output.display()))?;
    let status = runtime.finish_process();
    if status != 0 { return Err(format!("profiled program exited with status {status}")); }
    Ok(())
}

pub fn run_blob(runtime: &mut lumen_runtime::Runtime, path: &str) -> Option<Result<(), String>> {
    let native = match std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("lbc") => false,
        Some("lmc") => true,
        _ => return None,
    };
    Some(
        std::fs::read(path)
            .map_err(|e| format!("{path}: {e}"))
            .and_then(|bytes| if native { runtime.run_native_owned(bytes.into()) } else { runtime.run_precompiled_owned(bytes.into()) }),
    )
}
