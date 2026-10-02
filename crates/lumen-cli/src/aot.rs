use std::path::PathBuf;

pub fn write_target(args: &[String]) -> Result<(), String> {
    let [out] = args else {
        return Err("usage: lumen-cli target TARGET_FILE".into());
    };
    let bytes = lumen::target::host().encode()?;
    std::fs::write(out, bytes).map_err(|e| format!("{out}: {e}"))
}

pub fn compile(args: &[String]) -> Result<(), String> {
    let mut entry = None;
    let mut output = None;
    let mut tier = "bc";
    let mut target = None;
    let mut script = false;
    let mut spec = lumen_aot::build::Spec {
        walk: true,
        ..Default::default()
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--tier" => tier = iter.next().ok_or("--tier requires bc or mc")?,
            "-o" | "--output" => {
                output = Some(PathBuf::from(
                    iter.next().ok_or("--output requires a path")?,
                ))
            }
            "--target" => target = Some(iter.next().ok_or("--target requires a target file")?),
            "--script" => script = true,
            "--node-modules" => spec.node_modules = true,
            "--keep-source" => spec.keep_source.push("**".into()),
            "--help" | "-h" => {
                println!("usage: lumen-cli compile ENTRY --tier bc [-o APP.lbc] [--target TARGET_FILE] [--script] [--node-modules] [--keep-source]");
                return Ok(());
            }
            _ if arg.starts_with('-') => return Err(format!("unknown compile option: {arg}")),
            _ if entry.is_none() => entry = Some(PathBuf::from(arg)),
            _ => return Err(format!("unexpected compile argument: {arg}")),
        }
    }
    if tier != "bc" {
        return Err(if tier == "mc" {
            "AOT-MC code generation is not implemented".into()
        } else {
            format!("unknown AOT tier: {tier}")
        });
    }
    let entry = entry.ok_or("compile requires an entry file")?;
    if entry.extension().is_some_and(|e| e == "py") {
        return Err("Python AOT compilation is not linked".into());
    }
    if let Some(path) = target {
        let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let spec = lumen::target::TargetSpec::decode(&bytes)?;
        let host = lumen::target::host();
        if spec.profile == lumen::target::Profile::Aot
            || spec.bytecode_fp != host.bytecode_fp
            || spec.lumen_version != host.lumen_version
        {
            return Err("target cannot load this engine's AOT-BC blobs".into());
        }
    }
    let out = output.unwrap_or_else(|| entry.with_extension("lbc"));
    if script {
        spec.scripts.push(entry);
    } else {
        spec.entry = Some(entry);
    }
    let len = lumen_aot::build::compile_to(
        std::env::current_dir().map_err(|e| e.to_string())?,
        &spec,
        &out,
    )?;
    println!("{} ({} bytes, AOT-BC)", out.display(), len);
    Ok(())
}

pub fn run_blob(runtime: &mut lumen_runtime::Runtime, path: &str) -> Option<Result<(), String>> {
    if std::path::Path::new(path)
        .extension()
        .is_none_or(|e| e != "lbc")
    {
        return None;
    }
    Some(
        std::fs::read(path)
            .map_err(|e| format!("{path}: {e}"))
            .and_then(|bytes| runtime.run_precompiled_owned(bytes.into())),
    )
}
