//! The faces the engine falls back to when a page or canvas names none: Inconsolata for
//! `monospace` and Liberation Sans, regular and bold, for everything else.
//!
//! With `bundled-fonts` the faces are compiled in. Without it they are read once from the
//! directory given to [`set_font_directory`] (or the `LUMEN_FONT_DIR` environment variable),
//! which holds the files in [`FILES`].

use crate::FontFace;
use alloc::sync::Arc;
use std::path::PathBuf;
use std::sync::OnceLock;

/// File names of the three faces inside the font directory.
pub const FILES: [&str; 3] = ["Inconsolata-Regular.ttf", "LiberationSans-Regular.ttf", "LiberationSans-Bold.ttf"];

pub struct DefaultFaces {
    pub mono: Arc<FontFace>,
    pub sans: Arc<FontFace>,
    pub sans_bold: Arc<FontFace>,
}

static DIRECTORY: OnceLock<PathBuf> = OnceLock::new();
static FACES: OnceLock<DefaultFaces> = OnceLock::new();

/// Sets the directory the faces are read from. Only the first call counts; returns whether
/// this call set it. Unused with `bundled-fonts`.
pub fn set_font_directory(directory: impl Into<PathBuf>) -> bool {
    DIRECTORY.set(directory.into()).is_ok()
}

/// The shared default faces, loaded on first use. A failure is not cached, so a directory
/// set later is still picked up.
pub fn default_faces() -> Result<&'static DefaultFaces, &'static str> {
    if let Some(faces) = FACES.get() {
        return Ok(faces);
    }
    let loaded = load()?;
    Ok(FACES.get_or_init(|| loaded))
}

fn face(bytes: Arc<[u8]>) -> Result<Arc<FontFace>, &'static str> {
    FontFace::new(bytes).map(Arc::new)
}

#[cfg(feature = "bundled-fonts")]
fn load() -> Result<DefaultFaces, &'static str> {
    Ok(DefaultFaces {
        mono: face(Arc::from(crate::DEFAULT_FONT_BYTES))?,
        sans: face(Arc::from(crate::TEST_FONT_BYTES))?,
        sans_bold: face(Arc::from(crate::TEST_FONT_BOLD_BYTES))?,
    })
}

#[cfg(not(feature = "bundled-fonts"))]
fn load() -> Result<DefaultFaces, &'static str> {
    let environment = std::env::var_os("LUMEN_FONT_DIR").map(PathBuf::from);
    let directory = DIRECTORY
        .get()
        .cloned()
        .or(environment)
        .ok_or("font directory is not set (lumen_html_text::set_font_directory)")?;
    let read = |file: &str, error: &'static str| -> Result<Arc<FontFace>, &'static str> {
        let bytes = std::fs::read(directory.join(file)).map_err(|_| error)?;
        face(Arc::from(bytes))
    };
    Ok(DefaultFaces {
        mono: read(FILES[0], "cannot read Inconsolata-Regular.ttf from the font directory")?,
        sans: read(FILES[1], "cannot read LiberationSans-Regular.ttf from the font directory")?,
        sans_bold: read(FILES[2], "cannot read LiberationSans-Bold.ttf from the font directory")?,
    })
}
