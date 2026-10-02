use std::path::{Path, PathBuf};

pub(crate) fn collect(root: &Path, patterns: &[String]) -> Result<(Option<Vec<u8>>, Vec<PathBuf>), String> {
    if patterns.is_empty() { return Ok((None, Vec::new())); }
    let root = root.canonicalize().map_err(|error| format!("{}: {error}", root.display()))?;
    fn walk(root: &Path, directory: &Path, patterns: &[String], out: &mut Vec<(String, PathBuf, Vec<u8>)>) -> Result<(), String> {
        for entry in std::fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))? {
            let entry = entry.map_err(|error| error.to_string())?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_symlink() { continue; }
            let name = path.strip_prefix(root).map_err(|_| "asset escaped source root")?.to_str()
                .ok_or("asset path is not UTF-8")?.replace('\\', "/");
            if kind.is_dir() { walk(root, &path, patterns, out)?; }
            else if kind.is_file() && patterns.iter().any(|pattern| crate::walk::glob_match(pattern, &name)) {
                let bytes = std::fs::read(&path).map_err(|error| format!("{}: {error}", path.display()))?;
                out.push((name, path, bytes));
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    walk(&root, &root, patterns, &mut entries)?;
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    for pattern in patterns {
        if !entries.iter().any(|(name, _, _)| crate::walk::glob_match(pattern, name)) {
            return Err(format!("asset pattern matched no files: {pattern}"));
        }
    }
    let archive = lumen_common::aot::assets::encode(&entries.iter().map(|(name, _, bytes)| (name.as_str(), bytes.as_slice())).collect::<Vec<_>>())?;
    Ok((Some(archive), entries.into_iter().map(|(_, path, _)| path).collect()))
}
