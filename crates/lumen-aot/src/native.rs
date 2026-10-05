//! Host source orchestration for native images; runtime readiness is checked by the caller.

use crate::walk::{self, Spec};
use lumen::SourceKind;
use lumen_codegen::{aot_image, target_codegen};
use lumen_common::aot::{got::Kind, native_data::Import, Language};
use lumen_common::target::TargetSpec;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub struct Build {
    assets: Option<Vec<u8>>,
    pub image: aot_image::Image,
    pub payload: Vec<u8>,
    pub inputs: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub required_modules: Vec<String>,
    pub files: Vec<String>,
    pub locations: Vec<lumen_common::aot::native_lines::Location>,
    snapshot_offset: usize,
    pub snapshot_functions: lumen::precompiled::NativeSnapshotFunctions,
    pub snapshot_entry: Option<u32>,
    pub snapshot_units: Vec<(String, lumen::precompiled::CompiledUnit, Vec<(String, u32)>)>,
    pub snapshot_entry_unit: Option<u32>,
    pub trim_report: Vec<String>,
}

fn count(out: &mut Vec<u8>, value: usize) -> Result<(), String> {
    let value = u32::try_from(value).map_err(|_| "native metadata exceeds 32-bit limits")?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn string(out: &mut Vec<u8>, value: &str) -> Result<(), String> {
    count(out, value.len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

/// Build-script native bootstrap for a compilerless 64-bit target. The consuming
/// crate's CARGO_CFG_TARGET_* variables describe the target, not the host compiler.
pub fn precompile_glue_for_build(source: &str, name: &str) -> Result<Vec<u8>, String> {
    use lumen_common::target::{Abi, Arch, Placement, Profile};
    let arch_name = std::env::var("CARGO_CFG_TARGET_ARCH")
        .map_err(|_| "native glue requires CARGO_CFG_TARGET_ARCH")?;
    let os = std::env::var("CARGO_CFG_TARGET_OS")
        .map_err(|_| "native glue requires CARGO_CFG_TARGET_OS")?;
    let width = std::env::var("CARGO_CFG_TARGET_POINTER_WIDTH")
        .map_err(|_| "native glue requires CARGO_CFG_TARGET_POINTER_WIDTH")?;
    if width != "64" {
        return Err("native bootstrap glue requires a 64-bit target".into());
    }
    let arch = match arch_name.as_str() {
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        _ => {
            return Err(format!(
                "native bootstrap glue does not support architecture {arch_name}"
            ))
        }
    };
    let abi = match (arch, os.as_str()) {
        (Arch::X86_64 | Arch::Aarch64, "windows") => Abi::Win64,
        (Arch::X86_64, _) => Abi::SysV64,
        (Arch::Aarch64, "macos" | "ios") => Abi::Apple64,
        (Arch::Aarch64, _) => Abi::Aapcs64,
        _ => unreachable!(),
    };
    let mut target = TargetSpec {
        lumen_version: lumen::target::host().lumen_version,
        bytecode_fp: 0,
        native_fp: 0,
        arch,
        abi,
        pointer_width: 64,
        page_size: if arch == Arch::Aarch64 && matches!(os.as_str(), "macos" | "ios") {
            16384
        } else {
            4096
        },
        features: 0,
        builtin_modules_hash: if std::env::var_os("CARGO_FEATURE_PROCESSES").is_some() {
            lumen_common::aot::builtin_catalog::hash(lumen_common::aot::builtin_catalog::Features {
                parallel: true,
                bitnest_process: true,
                ..Default::default()
            })
        } else if std::env::var_os("CARGO_FEATURE_PARALLEL").is_some() {
            lumen::target::PARALLEL_BUILTIN_MODULES_HASH
        } else {
            lumen_common::aot::fingerprint::builtin_modules_hash(&[])
        },
        profile: Profile::Aot,
        code_placement: Placement::Ram,
    };
    target.native_fp = lumen::native_aot::fingerprint(&target);
    target.validate().map_err(str::to_owned)?;
    let unit = lumen::precompiled::CompiledUnit::compile_with_options(
        source,
        SourceKind::Script,
        lumen::precompiled::CompileOptions {
            bytecode: true,
            keep_source: false,
        },
    )?;
    let compiled = unit
        .compile_native(64, 0)
        .map_err(|error| format!("{name}: {error}"))?;
    let functions = compiled
        .functions
        .iter()
        .map(|function| {
            target_codegen::compile_aot(&function.function, &target, None, |id| {
                Some((Kind::Helper, id))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut payload = b"LUMJSN03".to_vec();
    for value in [
        3u32,
        functions
            .len()
            .try_into()
            .map_err(|_| "too many glue functions")?,
        1,
        0,
    ] {
        payload.extend_from_slice(&value.to_le_bytes());
    }
    string(&mut payload, name)?;
    for value in [0u32, 0, 0] {
        payload.extend_from_slice(&value.to_le_bytes());
    } // script, top function, links
    payload.extend(compiled.bindings);
    count(&mut payload, unit.requires().len())?;
    for require in unit.requires() {
        string(&mut payload, require)?;
    }
    payload.extend(compiled.metadata);
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&u32::MAX.to_le_bytes());
    aot_image::link(&functions)?.encode_native_with_imports(
        Language::JavaScript,
        &target,
        &[],
        &payload,
        None,
    )
}

/// Walk the same TS/JS/package graph as AOT-BC, then lower every unit and function.
/// The intermediate bytecode bundle is host-only and discarded.
pub fn compile(base: &Path, spec: &Spec, target: &TargetSpec) -> Result<Build, String> {
    compile_with_profile(base, spec, target, None)
}

pub fn compile_with_profile(
    base: &Path,
    spec: &Spec,
    target: &TargetSpec,
    profile: Option<&lumen::feedback::Profile>,
) -> Result<Build, String> {
    compile_with_options(base, spec, target, profile, None)
}

pub fn compile_with_options(
    base: &Path,
    spec: &Spec,
    target: &TargetSpec,
    profile: Option<&lumen::feedback::Profile>,
    snapshot_at: Option<&str>,
) -> Result<Build, String> {
    target.validate().map_err(str::to_owned)?;
    let mut spec = spec.clone();
    spec.closed_world = true;
    spec.walk = true;
    spec.no_bytecode = false;
    let mut functions = Vec::new();
    let mut records = Vec::new();
    let mut units = Vec::new();
    let mut source_paths = Vec::new();
    let mut source_locations = Vec::new();
    let mut snapshot_functions = lumen::precompiled::NativeSnapshotFunctions::default();
    let mut snapshot_entries = Vec::new();
    let mut snapshot_warnings = Vec::new();
    let mut snapshot_units = Vec::new();
    let mut trim_report = vec![format!(
        "trim: {}",
        spec.trim_level.as_deref().unwrap_or(if spec.trim_modules {
            "modules"
        } else {
            "false"
        })
    )];
    let bundle = walk::bundle_with(base, &spec, &mut |path, kind, unit, source| {
        if snapshot_at.is_some() {
            snapshot_warnings.extend(
                unit.native_snapshot_warnings()?
                    .into_iter()
                    .map(|warning| format!("{path}: {warning}")),
            );
        }
        let trimmed = spec
            .trim_level
            .as_deref()
            .filter(|level| matches!(*level, "members" | "aggressive"))
            .map(|_| {
                unit.trim_native_members(|name| {
                    snapshot_at.is_some()
                        || spec.keep.iter().any(|pattern| {
                            walk::glob_match(pattern, name)
                                || walk::glob_match(pattern, &format!("{path}.{name}"))
                        })
                })
            })
            .transpose()?;
        if let Some((_, removed, warnings)) = &trimmed {
            if spec.trim_level.as_deref() == Some("aggressive")
                && !warnings.is_empty()
                && !spec
                    .keep
                    .iter()
                    .any(|pattern| pattern == "**" || pattern == &format!("{path}.*"))
            {
                return Err(format!(
                    "{path}: aggressive trim requires --keep '**' or --keep '{path}.*': {}",
                    warnings.join("; ")
                ));
            }
            for name in removed {
                trim_report.push(format!("trimmed: {path}.{name}"));
            }
            trim_report.push(format!("removed: {path}: {} declarations, {} function-source bytes; reason: unreachable from top-level, exports and keep roots", removed.len(), unit.native_trim_source_bytes(removed)));
            for warning in warnings {
                trim_report.push(format!("warning: {path}: {warning}"));
            }
            if snapshot_at.is_some() {
                trim_report.push(format!(
                    "warning: {path}: snapshot lexical environments retain function declarations"
                ));
            }
        }
        let unit = trimmed.as_ref().map(|(unit, _, _)| unit).unwrap_or(unit);
        if snapshot_at.is_some() {
            snapshot_units.push((path.to_owned(), unit.clone(), Vec::new()));
        }
        let first = u32::try_from(functions.len()).map_err(|_| "too many native functions")?;
        let compiled = unit
            .compile_native_with_profile(target.pointer_width, first, profile)
            .map_err(|e| format!("{path}: {e}"))?;
        trim_report.push(format!(
            "kept: {path} ({} native functions, {} metadata bytes)",
            compiled.functions.len(),
            compiled.metadata.len()
        ));
        snapshot_functions.extend(compiled.snapshot_functions);
        snapshot_entries.push(snapshot_at.map(|name| unit.native_snapshot_entry(name, first)));
        for function in compiled.functions {
            functions.push(target_codegen::compile_aot(
                &function.function,
                target,
                None,
                |id| Some((Kind::Helper, id)),
            )?);
        }
        let file = source_paths.len();
        source_paths.push(
            source
                .canonicalize()
                .map_err(|e| format!("{}: {e}", source.display()))?,
        );
        for (local, location) in compiled.locations.into_iter().enumerate() {
            if let Some((line, column)) = location {
                source_locations.push((first as usize + local, file, line, column));
            }
        }
        units.push((
            path.to_owned(),
            kind,
            first,
            compiled.bindings,
            unit.requires().to_vec(),
        ));
        records.extend(compiled.metadata);
        Ok(())
    })?;
    for (from, specifier, to) in &bundle.links {
        if let Some((_, _, links)) = snapshot_units.get_mut(*from) {
            links.push((
                specifier.clone(),
                u32::try_from(*to).map_err(|_| "too many snapshot units")?,
            ));
        }
    }
    let snapshot_entry_unit = bundle
        .entry
        .map(|entry| u32::try_from(entry).map_err(|_| "too many snapshot units"))
        .transpose()?;
    let known_catalog = lumen_common::aot::builtin_catalog::known(target.builtin_modules_hash)
        .map(lumen_common::aot::builtin_catalog::entries);
    let catalog = if let Some(catalog) = &spec.native_catalog {
        let entries = catalog
            .iter()
            .map(|(module, name, signature)| (module.as_str(), name.as_str(), *signature))
            .collect::<Vec<_>>();
        if lumen_common::aot::fingerprint::builtin_modules_hash(&entries)
            != target.builtin_modules_hash
        {
            return Err(
                "native builtin catalog does not match the target's advertised table hash".into(),
            );
        }
        catalog
            .iter()
            .map(|(module, _, _)| module.as_str())
            .collect::<BTreeSet<_>>()
    } else if let Some(catalog) = &known_catalog {
        catalog
            .iter()
            .map(|(module, _, _)| module.as_str())
            .collect::<BTreeSet<_>>()
    } else if target.builtin_modules_hash
        == lumen_common::aot::fingerprint::builtin_modules_hash(&[])
        || bundle.required_modules.is_empty()
    {
        BTreeSet::new()
    } else {
        return Err("custom native builtin table requires --builtin-catalog; no namespaces are inferred from a table hash".into());
    };
    for module in &bundle.required_modules {
        if !catalog.contains(module.as_str()) {
            return Err(format!("native target does not declare namespace {module:?}; source-backed builtin modules cannot be loaded by AOT-MC"));
        }
    }
    let mut payload = b"LUMJSN03".to_vec();
    payload.extend_from_slice(&3u32.to_le_bytes());
    count(&mut payload, functions.len())?;
    count(&mut payload, units.len())?;
    payload.extend_from_slice(
        &bundle
            .entry
            .or_else(|| (units.len() == 1).then_some(0))
            .map_or(u32::MAX, |i| i as u32)
            .to_le_bytes(),
    );
    for (index, (path, kind, entry, bindings, requires)) in units.iter().enumerate() {
        string(&mut payload, path)?;
        let kind: u32 = match kind {
            SourceKind::Script => 0,
            SourceKind::Module => 1,
            SourceKind::CommonJs => 2,
        };
        payload.extend_from_slice(&kind.to_le_bytes());
        payload.extend_from_slice(&entry.to_le_bytes());
        let links = bundle
            .links
            .iter()
            .filter(|(from, _, _)| *from == index)
            .collect::<Vec<_>>();
        count(&mut payload, links.len())?;
        for (_, name, to) in links {
            string(&mut payload, name)?;
            count(&mut payload, *to)?;
        }
        payload.extend_from_slice(bindings);
        count(&mut payload, requires.len())?;
        for require in requires {
            string(&mut payload, require)?;
        }
    }
    payload.extend(records);
    let snapshot_offset = payload.len();
    payload.extend_from_slice(&0u32.to_le_bytes());
    payload.extend_from_slice(&u32::MAX.to_le_bytes());
    let snapshot_entry = if snapshot_at.is_some() {
        let unit = bundle
            .entry
            .or_else(|| (units.len() == 1).then_some(0))
            .ok_or("snapshot requires one script or an entry module")?;
        let entry = snapshot_entries[unit]
            .take()
            .ok_or("missing snapshot entry")?;
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let mut linked = bundle
                    .links
                    .iter()
                    .filter(|(from, _, to)| *from == unit && units[*to].1 == SourceKind::CommonJs)
                    .map(|(_, _, to)| *to);
                let target = linked.next().ok_or(error)?;
                if linked.next().is_some() {
                    return Err("snapshot entry facade has multiple CommonJS targets".into());
                }
                snapshot_entries[target]
                    .take()
                    .ok_or("missing CommonJS snapshot entry")??
            }
        };
        Some(entry)
    } else {
        None
    };
    let mut root = if let Some(root) = &spec.root {
        base.join(root)
            .canonicalize()
            .map_err(|e| format!("{}: {e}", root.display()))?
    } else {
        source_paths
            .first()
            .and_then(|path| path.parent())
            .unwrap_or(base)
            .to_path_buf()
    };
    if spec.root.is_none() {
        while source_paths.iter().any(|path| !path.starts_with(&root)) {
            if !root.pop() {
                return Err("native sources have no common root".into());
            }
        }
    }
    let paths = source_paths
        .iter()
        .map(|path| {
            path.strip_prefix(&root)
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .map_err(|_| "native source is outside the source root".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let files = paths
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let locations = source_locations
        .into_iter()
        .map(|(function, file, line, column)| {
            Ok(lumen_common::aot::native_lines::Location {
                function: u32::try_from(function).map_err(|_| "too many native functions")?,
                code_offset: 0,
                file: u32::try_from(files.binary_search(&paths[file]).unwrap())
                    .map_err(|_| "too many native source files")?,
                line,
                column,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut warnings = bundle.warnings;
    warnings.extend(snapshot_warnings);
    let image = aot_image::link(&functions)?;
    trim_report.push(format!("kept native code: {} bytes; function extents: {} bytes; metadata: {} bytes; reason: entry/export/transitive/keep roots and conservative observable members",
        image.code.len(), image.functions.iter().map(|function| function.len as usize).sum::<usize>(), payload.len()));
    Ok(Build {
        assets: bundle.assets,
        image,
        payload,
        inputs: bundle.inputs,
        warnings,
        required_modules: bundle.required_modules,
        files,
        locations,
        snapshot_offset,
        snapshot_functions,
        snapshot_entry,
        snapshot_units,
        snapshot_entry_unit,
        trim_report,
    })
}

impl Build {
    /// Attach a portable initialized heap graph after the native function records.
    pub fn attach_snapshot(
        &mut self,
        snapshot: &lumen::heap_snapshot::Snapshot,
    ) -> Result<(), String> {
        if self.payload.get(..8) != Some(b"LUMJSN03")
            || self.payload.len() != self.snapshot_offset + 8
            || self.payload[self.snapshot_offset..self.snapshot_offset + 4] != [0; 4]
        {
            return Err(
                "native build already contains a snapshot or has an invalid metadata version"
                    .into(),
            );
        }
        let bytes = snapshot.encode()?;
        let length =
            u32::try_from(bytes.len()).map_err(|_| "native snapshot exceeds 32-bit limits")?;
        let entry = self
            .snapshot_entry
            .filter(|entry| (*entry as usize) < self.image.functions.len())
            .ok_or("snapshot requires a valid native entry function")?;
        use lumen::heap_snapshot::{Call, Export, Node};
        let mut stack = snapshot
            .roots
            .iter()
            .chain(&snapshot.environments)
            .map(|(_, node)| *node)
            .collect::<Vec<_>>();
        let mut seen = vec![false; snapshot.nodes.len()];
        let mut entry_environments = BTreeSet::new();
        fn properties(stack: &mut Vec<u32>, properties: &[lumen::heap_snapshot::Property]) {
            for property in properties {
                stack.extend(property.symbol);
                if property.flags & 1 == 0 {
                    stack.push(property.value);
                }
                stack.extend(property.getter);
                stack.extend(property.setter);
            }
        }
        while let Some(index) = stack.pop() {
            let visited = seen
                .get_mut(index as usize)
                .ok_or("snapshot reference is out of range")?;
            if *visited {
                continue;
            }
            *visited = true;
            match &snapshot.nodes[index as usize] {
                Node::Object {
                    prototype,
                    callable,
                    properties: props,
                    exports,
                    class,
                    ..
                } => {
                    stack.extend(*prototype);
                    properties(&mut stack, props);
                    match callable {
                        Call::Native {
                            function,
                            environment,
                        } => {
                            if *function as usize >= self.image.functions.len()
                                || !matches!(
                                    snapshot.nodes.get(*environment as usize),
                                    Some(Node::Environment { .. })
                                )
                            {
                                return Err("snapshot native closure references an invalid function or environment".into());
                            }
                            stack.push(*environment);
                            if *function == entry {
                                entry_environments.insert(*environment);
                            }
                        }
                        Call::Bound {
                            target,
                            this,
                            arguments,
                        } => {
                            stack.extend([*target, *this]);
                            stack.extend(arguments);
                        }
                        _ => {}
                    }
                    if let Some(exports) = exports {
                        for (_, export) in exports {
                            match export {
                                Export::Live { environment, .. } => stack.push(*environment),
                                Export::Static(value) => stack.push(*value),
                            }
                        }
                    }
                    if let Some(class) = class {
                        stack.push(class.environment);
                        stack.extend(class.body);
                        stack.extend(&class.initializers);
                        properties(&mut stack, &class.private_members);
                        for field in &class.fields {
                            stack.extend(field.initializer);
                            stack.extend(&field.transforms);
                        }
                    }
                }
                Node::Environment {
                    parent,
                    with_object,
                    bindings,
                    ..
                } => {
                    stack.extend(*parent);
                    stack.extend(*with_object);
                    for binding in bindings {
                        if let Some((environment, _)) = &binding.import {
                            stack.push(*environment);
                        } else {
                            stack.push(binding.value);
                        }
                    }
                }
                _ => {}
            }
        }
        if entry_environments.len() != 1 {
            return Err(
                "snapshot entry requires exactly one reachable native closure environment".into(),
            );
        }
        self.payload.truncate(self.snapshot_offset);
        self.payload.extend_from_slice(&length.to_le_bytes());
        self.payload.extend(bytes);
        self.payload.extend_from_slice(&entry.to_le_bytes());
        Ok(())
    }
    pub fn encode(&self, target: &TargetSpec) -> Result<Vec<u8>, String> {
        let imports = self
            .required_modules
            .iter()
            .map(|module| Import {
                module,
                name: "",
                signature_hash: 0,
            })
            .collect::<Vec<_>>();
        let blob = self.image.encode_native_with_locations(
            Language::JavaScript,
            target,
            &imports,
            &self.payload,
            &self.files.iter().map(String::as_str).collect::<Vec<_>>(),
            &self.locations,
        )?;
        match &self.assets {
            Some(archive) => {
                lumen_common::aot::assets::attach(&blob, archive, target.page_size as usize)
                    .map_err(String::from)
            }
            None => Ok(blob),
        }
    }

    pub fn encode_stripped(&self, target: &TargetSpec) -> Result<(Vec<u8>, Vec<u8>), String> {
        let imports = self
            .required_modules
            .iter()
            .map(|module| Import {
                module,
                name: "",
                signature_hash: 0,
            })
            .collect::<Vec<_>>();
        let (blob, map) = self.image.encode_native_stripped(
            Language::JavaScript,
            target,
            &imports,
            &self.payload,
            self.files.clone(),
            self.locations.clone(),
        )?;
        if let Some(archive) = &self.assets {
            let blob =
                lumen_common::aot::assets::attach(&blob, archive, target.page_size as usize)?;
            let mut map = lumen_common::aot::sidecar::Sidecar::decode(&map)?;
            map.blob_hash = lumen_common::aot::sidecar::hash(&blob);
            Ok((blob, map.encode()?))
        } else {
            Ok((blob, map))
        }
    }
}
