//! Font files installed on the host: the platform font directories and a bounded walk over them.
//! Reading and interpreting the files belongs to the text engine; this module only finds them.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Deepest directory level the walk descends below a font directory.
pub const MAX_DEPTH: usize = 6;
/// Most font files one walk returns.
pub const MAX_FILES: usize = 20_000;
/// Most distinct directories one walk visits.
pub const MAX_DIRECTORIES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FontPlatform {
    MacOs,
    Linux,
    Windows,
}

impl FontPlatform {
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

/// The environment the platform directories depend on; `current` reads the process environment.
#[derive(Clone, Debug, Default)]
pub struct FontEnvironment {
    pub home: Option<PathBuf>,
    pub windir: Option<PathBuf>,
    pub local_app_data: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
    pub xdg_data_dirs: Vec<PathBuf>,
}

impl FontEnvironment {
    pub fn current() -> Self {
        let var = |name| std::env::var_os(name).filter(|value| !value.is_empty()).map(PathBuf::from);
        Self {
            home: var("HOME").or_else(|| var("USERPROFILE")),
            windir: var("WINDIR").or_else(|| var("SystemRoot")),
            local_app_data: var("LOCALAPPDATA"),
            xdg_data_home: var("XDG_DATA_HOME"),
            xdg_data_dirs: std::env::var_os("XDG_DATA_DIRS")
                .map(|dirs| std::env::split_paths(&dirs).filter(|path| path.is_absolute()).collect())
                .unwrap_or_default(),
        }
    }
}

/// Font directories of `platform`, most specific system location first. Directories that do not
/// exist are included; the walk skips them.
pub fn font_directories(platform: FontPlatform, environment: &FontEnvironment) -> Vec<PathBuf> {
    let home = environment.home.as_deref();
    let mut directories: Vec<PathBuf> = Vec::new();
    match platform {
        FontPlatform::MacOs => {
            for fixed in [
                "/System/Library/Fonts",
                "/System/Library/Fonts/Supplemental",
                "/Library/Fonts",
                // PingFang is only installed here or as a downloaded font asset.
                "/System/Library/PrivateFrameworks/FontServices.framework/Versions/A/Resources/Reserved",
            ] {
                directories.push(PathBuf::from(fixed));
            }
            if let Ok(entries) = std::fs::read_dir("/System/Library/AssetsV2") {
                let mut assets: Vec<PathBuf> = entries
                    .flatten()
                    .filter(|entry| entry.file_name().to_string_lossy().starts_with("com_apple_MobileAsset_Font"))
                    .map(|entry| entry.path())
                    .collect();
                assets.sort();
                directories.extend(assets);
            }
            if let Some(home) = home {
                directories.push(home.join("Library/Fonts"));
            }
        }
        FontPlatform::Linux => {
            directories.push(PathBuf::from("/usr/share/fonts"));
            directories.push(PathBuf::from("/usr/local/share/fonts"));
            let data_home = environment.xdg_data_home.clone().or_else(|| home.map(|home| home.join(".local/share")));
            let data_dirs = data_home.into_iter().chain(environment.xdg_data_dirs.iter().cloned());
            for data_dir in data_dirs {
                let fonts = data_dir.join("fonts");
                if !directories.contains(&fonts) {
                    directories.push(fonts);
                }
            }
            if let Some(home) = home {
                directories.push(home.join(".fonts"));
            }
        }
        FontPlatform::Windows => {
            let windir = environment.windir.clone().unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
            directories.push(windir.join("Fonts"));
            if let Some(local) = &environment.local_app_data {
                directories.push(local.join(r"Microsoft\Windows\Fonts"));
            }
        }
    }
    directories
}

/// Whether the path has a font file extension this engine reads (`.ttf .otf .ttc .otc`).
pub fn is_font_file(path: &Path) -> bool {
    path.extension().and_then(OsStr::to_str).is_some_and(|extension| {
        ["ttf", "otf", "ttc", "otc"].iter().any(|font| extension.eq_ignore_ascii_case(font))
    })
}

/// Font files below `directories`, each directory in order and its entries sorted by name, so the
/// result is stable. A file reachable from two directories appears once.
pub fn list_font_files(directories: &[PathBuf], max_depth: usize, max_files: usize) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut visited = HashSet::new();
    for directory in directories {
        walk(directory, 0, max_depth, max_files, &mut files, &mut seen, &mut visited);
        if files.len() >= max_files {
            break;
        }
    }
    files
}

fn walk(
    directory: &Path,
    depth: usize,
    max_depth: usize,
    max_files: usize,
    files: &mut Vec<PathBuf>,
    seen: &mut HashSet<PathBuf>,
    visited: &mut HashSet<PathBuf>,
) {
    // Canonical paths make symlink loops and directories reachable twice walk once.
    let Ok(canonical) = std::fs::canonicalize(directory) else { return };
    if visited.len() >= MAX_DIRECTORIES || !visited.insert(canonical) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else { return };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        if files.len() >= max_files {
            return;
        }
        let Ok(metadata) = std::fs::metadata(&path) else { continue };
        if metadata.is_dir() {
            if depth < max_depth {
                walk(&path, depth + 1, max_depth, max_files, files, seen, visited);
            }
        } else if metadata.is_file() && is_font_file(&path) && seen.insert(path.clone()) {
            files.push(path);
        }
    }
}

/// Every font file installed for this host.
pub fn system_font_files() -> Vec<PathBuf> {
    let directories = font_directories(FontPlatform::current(), &FontEnvironment::current());
    list_font_files(&directories, MAX_DEPTH, MAX_FILES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_are_case_insensitive_and_exclusive() {
        for name in ["a.ttf", "a.OTF", "a.ttc", "a.Otc"] {
            assert!(is_font_file(Path::new(name)), "{name}");
        }
        for name in ["a.woff", "a.pfb", "a", "ttf", "a.dfont"] {
            assert!(!is_font_file(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn platform_directories() {
        let environment = FontEnvironment {
            home: Some(PathBuf::from("/home/u")),
            windir: Some(PathBuf::from(r"D:\Win")),
            local_app_data: Some(PathBuf::from(r"D:\Local")),
            ..FontEnvironment::default()
        };
        let linux = font_directories(FontPlatform::Linux, &environment);
        assert_eq!(linux.len(), 4);
        assert!(linux.contains(&PathBuf::from("/home/u/.fonts")));
        let xdg = FontEnvironment {
            xdg_data_home: Some(PathBuf::from("/data")),
            xdg_data_dirs: vec![PathBuf::from("/usr/share"), PathBuf::from("/opt/share")],
            ..environment.clone()
        };
        let linux = font_directories(FontPlatform::Linux, &xdg);
        assert!(linux.contains(&PathBuf::from("/data/fonts")));
        assert!(linux.contains(&PathBuf::from("/opt/share/fonts")));
        assert!(!linux.contains(&PathBuf::from("/home/u/.local/share/fonts")));
        assert_eq!(linux.iter().filter(|path| **path == PathBuf::from("/usr/share/fonts")).count(), 1);
        let mac = font_directories(FontPlatform::MacOs, &environment);
        assert!(mac.contains(&PathBuf::from("/System/Library/Fonts/Supplemental")));
        assert!(mac.contains(&PathBuf::from("/home/u/Library/Fonts")));
        let windows = font_directories(FontPlatform::Windows, &environment);
        assert_eq!(windows[0], PathBuf::from(r"D:\Win").join("Fonts"));
        assert_eq!(windows.len(), 2);
        assert!(font_directories(FontPlatform::Linux, &FontEnvironment::default()).len() == 2);
    }

    #[test]
    fn walk_is_bounded_sorted_and_deduplicated() {
        let root = std::env::temp_dir().join(format!("lumen-os-fonts-{}", std::process::id()));
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        for file in ["z.ttf", "m.txt", "a.otf", "a/one.ttc", "a/b/two.ttf", "a/b/c/three.ttf"] {
            std::fs::write(root.join(file), b"x").unwrap();
        }
        let names = |files: Vec<PathBuf>| {
            files.iter().map(|path| path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/")).collect::<Vec<_>>()
        };
        let all = list_font_files(&[root.clone(), root.join("a")], 6, 100);
        assert_eq!(names(all), ["a/b/c/three.ttf", "a/b/two.ttf", "a/one.ttc", "a.otf", "z.ttf"]);
        let shallow = list_font_files(&[root.clone()], 1, 100);
        assert_eq!(names(shallow), ["a/one.ttc", "a.otf", "z.ttf"]);
        assert_eq!(list_font_files(&[root.clone()], 6, 2).len(), 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_loops_are_walked_once() {
        let root = std::env::temp_dir().join(format!("lumen-os-fonts-loop-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::write(root.join("a/one.ttf"), b"x").unwrap();
        std::os::unix::fs::symlink(&root, root.join("a/back")).unwrap();
        std::os::unix::fs::symlink(root.join("a"), root.join("alias")).unwrap();
        let files = list_font_files(&[root.clone()], 6, 100);
        assert_eq!(files.len(), 1);
        let _ = std::fs::remove_file(root.join("a/back"));
        let _ = std::fs::remove_file(root.join("alias"));
        let _ = std::fs::remove_file(root.join("a/one.ttf"));
        let _ = std::fs::remove_dir(root.join("a"));
        let _ = std::fs::remove_dir(&root);
    }
}
