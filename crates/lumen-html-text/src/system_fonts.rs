//! Fonts installed on the host: a lazily built index of family names and styles read from the
//! sfnt directory, `name`, `OS/2`, `head` and `fvar` tables of every installed face, CSS family and
//! generic-family matching against that index, and per-character fallback. Faces are loaded only
//! when text uses them.
//!
//! Hosts opt in with [`enable_system_fonts`]; until then the engine behaves exactly as without
//! this module.

use crate::sfnt::{be16, be32, standalone_length, SfntWriter};
use crate::{FontFace, RegisteredFont, RegisteredFontDescriptors, MAX_SYSTEM_FONT_BYTES};
use alloc::sync::Arc;
use lumen_common::bidi::{Script, UnicodeScript};
use lumen_html::paint::{font_match_style_range_rank, FontSpec, FontStyle, GenericFontFamily};
use lumen_os::fonts::FontPlatform;
use read_fonts::types::Tag;
use read_fonts::{FontData, FontRead};
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::prelude::v1::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::{format, vec};

/// Face indices at or above this value name a system face; below it they index a `FontSet`.
pub(crate) const SYSTEM_FACE_BASE: usize = 1 << 24;

const MAX_TABLE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_FACES_PER_FILE: usize = 512;
const MAX_NAME_KEYS: usize = 24;
const MAX_LOADED_FACES: usize = 48;
const MAX_LOADED_BYTES: usize = 512 * 1024 * 1024;
const MAX_CMAP_CACHE: usize = 128;
const MAX_FALLBACK_CACHE: usize = 32768;
const SCAN_LIST_LEN: usize = 48;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Lets this process use the fonts installed on the host: named families the page does not
/// define, `system-ui` and the other generic families, and per-character fallback. Hosts call it
/// once at startup; without it the bundled faces and `@font-face` fonts are all that is used.
pub fn enable_system_fonts() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn system_fonts_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Clone, Copy)]
struct TableRecord {
    tag: [u8; 4],
    checksum: u32,
    offset: u64,
    length: u32,
}

struct SfntFace {
    version: [u8; 4],
    offset: u64,
    tables: Vec<TableRecord>,
}

fn read_at<R: Read + Seek>(reader: &mut R, at: u64, length: usize) -> io::Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(at))?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_face<R: Read + Seek>(reader: &mut R, offset: u64, file_length: u64) -> io::Result<SfntFace> {
    let header = read_at(reader, offset, 12)?;
    let version: [u8; 4] = header[..4].try_into().map_err(|_| invalid("short sfnt header"))?;
    if ![[0, 1, 0, 0], *b"OTTO", *b"true", *b"typ1"].contains(&version) {
        return Err(invalid("not an sfnt font"));
    }
    let count = usize::from(be16(&header, 4).ok_or_else(|| invalid("short sfnt header"))?);
    if count == 0 || count > 256 {
        return Err(invalid("bad table count"));
    }
    let directory = read_at(reader, offset + 12, count * 16)?;
    let mut tables = Vec::with_capacity(count);
    for record in directory.chunks_exact(16) {
        let table = TableRecord {
            tag: record[..4].try_into().map_err(|_| invalid("short table record"))?,
            checksum: be32(record, 4).unwrap_or(0),
            offset: u64::from(be32(record, 8).unwrap_or(0)),
            length: be32(record, 12).unwrap_or(0),
        };
        if table.offset + u64::from(table.length) > file_length {
            return Err(invalid("table outside file"));
        }
        tables.push(table);
    }
    Ok(SfntFace { version, offset, tables })
}

/// Every face of a font file: the members of a collection, or the one face of a plain font.
fn read_faces<R: Read + Seek>(reader: &mut R, file_length: u64) -> io::Result<Vec<SfntFace>> {
    let head = read_at(reader, 0, 12)?;
    if &head[..4] != b"ttcf" {
        return Ok(vec![read_face(reader, 0, file_length)?]);
    }
    let fitting = (file_length.saturating_sub(12) / 4).min(MAX_FACES_PER_FILE as u64) as usize;
    let count = (be32(&head, 8).unwrap_or(0) as usize).min(fitting);
    let offsets = read_at(reader, 12, count * 4)?;
    let mut faces = Vec::new();
    for offset in offsets.chunks_exact(4) {
        let offset = u64::from(be32(offset, 0).unwrap_or(0));
        if let Ok(face) = read_face(reader, offset, file_length) {
            faces.push(face);
        }
    }
    Ok(faces)
}

fn table_bytes<R: Read + Seek>(reader: &mut R, face: &SfntFace, tag: &[u8; 4]) -> Option<Vec<u8>> {
    let record = face.tables.iter().find(|record| &record.tag == tag)?;
    if u64::from(record.length) > MAX_TABLE_BYTES {
        return None;
    }
    read_at(reader, record.offset, record.length as usize).ok()
}

/// One face of a collection as a standalone sfnt, read with seeks so the other members never
/// enter memory.
fn assemble_standalone<R: Read + Seek>(reader: &mut R, face: &SfntFace) -> io::Result<Vec<u8>> {
    let mut writer = SfntWriter::new(face.version, face.tables.len()).map_err(invalid)?;
    for (position, table) in face.tables.iter().enumerate() {
        if !writer.fits(table.length as usize) {
            return Err(invalid("font too large"));
        }
        let data = read_at(reader, table.offset, table.length as usize)?;
        writer.push_table(position, table.tag, table.checksum, &data).map_err(invalid)?;
    }
    Ok(writer.into_bytes())
}

/// Family names of a `name` table: the display name (English preferred, typographic family before
/// legacy family) and every lowercase spelling, in every language, that CSS may match.
fn family_names(table: &[u8]) -> Option<(String, Vec<String>)> {
    use read_fonts::tables::name::Name;
    let name = Name::read(FontData::new(table)).ok()?;
    let strings = name.string_data();
    let mut display: [Option<String>; 4] = [None, None, None, None];
    let mut keys: Vec<String> = Vec::new();
    for record in name.name_record() {
        let typographic = match record.name_id().to_u16() {
            16 => true,
            1 => false,
            _ => continue,
        };
        let Ok(string) = record.string(strings) else { continue };
        let text: String = string.chars().collect();
        let text = text.trim();
        if text.is_empty() || text.len() > 128 {
            continue;
        }
        let english = (record.platform_id() == 3 && record.language_id() == 0x409)
            || (record.platform_id() == 1 && record.language_id() == 0);
        let slot = usize::from(!english) * 2 + usize::from(!typographic);
        display[slot].get_or_insert_with(|| text.to_owned());
        let key = text.to_lowercase();
        if keys.len() < MAX_NAME_KEYS && !keys.contains(&key) {
            keys.push(key);
        }
    }
    let display = display.into_iter().flatten().next()?;
    Some((display, keys))
}

/// `usWidthClass` as the CSS `font-stretch` percentage.
fn width_percent(class: u16) -> f32 {
    match class {
        1 => 50.0,
        2 => 62.5,
        3 => 75.0,
        4 => 87.5,
        6 => 112.5,
        7 => 125.0,
        8 => 150.0,
        9 => 200.0,
        _ => 100.0,
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SystemFontEntry {
    pub family: Arc<str>,
    pub keys: Vec<Arc<str>>,
    pub weight: u16,
    pub weight_range: [u16; 2],
    pub stretch_range: [f32; 2],
    pub style: FontStyle,
    pub path: Arc<Path>,
    pub collection: bool,
    pub face_offset: u64,
    pub file_length: u64,
    /// Bytes the loaded face occupies: the whole file, or the standalone face of a collection.
    pub face_length: u64,
    pub cmap: Option<(u64, u32)>,
}

fn describe_face<R: Read + Seek>(
    reader: &mut R,
    face: &SfntFace,
    path: &Arc<Path>,
    collection: bool,
    file_length: u64,
) -> Option<SystemFontEntry> {
    let has = |tag: &[u8; 4]| face.tables.iter().any(|record| &record.tag == tag);
    // The rasterizer draws outlines only: bitmap and bitmap-only color fonts would show blanks.
    if has(b"sbix") || has(b"CBDT") || !(has(b"glyf") || has(b"CFF ") || has(b"CFF2")) {
        return None;
    }
    let (display, mut keys) = table_bytes(reader, face, b"name").and_then(|table| family_names(&table))?;
    if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
        let stem = stem.to_lowercase();
        if !keys.contains(&stem) {
            keys.push(stem);
        }
    }
    let mut weight = 400;
    let mut width = 100.0;
    let mut style = FontStyle::Normal;
    match table_bytes(reader, face, b"OS/2").and_then(|table| read_fonts::tables::os2::Os2::read(FontData::new(&table)).ok().map(|os2| {
        (os2.us_weight_class(), os2.us_width_class(), os2.fs_selection().bits())
    })) {
        Some((class, width_class, selection)) => {
            weight = match class {
                0 => 400,
                1..=9 => class * 100,
                other => other.min(1000),
            };
            width = width_percent(width_class);
            style = if selection & 0x200 != 0 {
                FontStyle::Oblique
            } else if selection & 1 != 0 {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            };
        }
        None => {
            if let Some(mac_style) = table_bytes(reader, face, b"head").and_then(|head| be16(&head, 44)) {
                weight = if mac_style & 1 != 0 { 700 } else { 400 };
                if mac_style & 2 != 0 {
                    style = FontStyle::Italic;
                }
            }
        }
    }
    let mut weight_range = [weight, weight];
    let mut stretch_range = [width, width];
    if let Some(fvar) = table_bytes(reader, face, b"fvar") {
        if let Ok(axes) = read_fonts::tables::fvar::Fvar::read(FontData::new(&fvar)).and_then(|fvar| fvar.axis_instance_arrays()) {
            for axis in axes.axes() {
                let (low, high) = (axis.min_value().to_f32(), axis.max_value().to_f32());
                if axis.axis_tag() == Tag::new(b"wght") && low >= 1.0 && high <= 1000.0 && low <= high {
                    weight_range = [low.round() as u16, high.round() as u16];
                } else if axis.axis_tag() == Tag::new(b"wdth") && low >= 0.0 && low <= high {
                    stretch_range = [low, high];
                }
            }
        }
    }
    let cmap = face.tables.iter().find(|record| &record.tag == b"cmap").map(|record| (record.offset, record.length));
    Some(SystemFontEntry {
        family: Arc::from(display),
        keys: keys.into_iter().map(Arc::from).collect(),
        weight,
        weight_range,
        stretch_range,
        style,
        path: path.clone(),
        collection,
        face_offset: face.offset,
        file_length,
        face_length: if collection { standalone_length(face.tables.iter().map(|record| record.length)) } else { file_length },
        cmap,
    })
}

/// The faces of one font file; unreadable or unsupported files give none.
fn scan_reader<R: Read + Seek>(reader: &mut R, path: &Arc<Path>, file_length: u64) -> Vec<SystemFontEntry> {
    let Ok(faces) = read_faces(reader, file_length) else { return Vec::new() };
    let collection = faces.len() > 1 || faces.first().is_some_and(|face| face.offset != 0);
    faces
        .iter()
        .filter_map(|face| describe_face(reader, face, path, collection, file_length))
        .collect()
}

pub(crate) struct SystemFontIndex {
    entries: Vec<SystemFontEntry>,
    by_key: HashMap<Arc<str>, Vec<u32>>,
}

impl SystemFontIndex {
    pub(crate) fn from_entries(entries: Vec<SystemFontEntry>) -> Self {
        let mut by_key: HashMap<Arc<str>, Vec<u32>> = HashMap::new();
        for (position, entry) in entries.iter().enumerate() {
            for key in &entry.keys {
                by_key.entry(key.clone()).or_default().push(position as u32);
            }
        }
        Self { entries, by_key }
    }

    pub(crate) fn scan(files: &[PathBuf]) -> Self {
        let mut entries = Vec::new();
        for file in files {
            let Ok(mut handle) = std::fs::File::open(file) else { continue };
            let Ok(length) = handle.metadata().map(|metadata| metadata.len()) else { continue };
            let path: Arc<Path> = Arc::from(file.as_path());
            entries.extend(scan_reader(&mut handle, &path, length));
        }
        Self::from_entries(entries)
    }

    /// The face of the family `key` (lowercase) that CSS font matching picks for `spec`.
    pub(crate) fn best_match(&self, key: &str, spec: &FontSpec) -> Option<usize> {
        self.by_key
            .get(key)?
            .iter()
            .filter_map(|&position| {
                let entry = &self.entries[position as usize];
                let (rank, ..) = font_match_style_range_rank(spec, entry.style, None, entry.weight_range, entry.stretch_range)?;
                Some((rank, position))
            })
            .min()
            .map(|(_, position)| position as usize)
    }

    fn is_last_resort(entry: &SystemFontEntry) -> bool {
        entry.keys.iter().any(|key| key.replace(' ', "") == "lastresort")
    }

    /// Faces most likely to cover an arbitrary character: the largest character maps.
    fn scan_list(&self) -> Vec<u32> {
        let mut candidates: Vec<(u32, u32)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| !Self::is_last_resort(entry))
            .filter_map(|(position, entry)| Some((entry.cmap?.1, position as u32)))
            .collect();
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        candidates.into_iter().take(SCAN_LIST_LEN).map(|(_, position)| position).collect()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum Lang {
    Unspecified,
    ZhHans,
    ZhHant,
    Ja,
    Ko,
    Other,
}

pub(crate) fn lang_class(language: Option<&str>) -> Lang {
    let Some(language) = language.filter(|language| !language.is_empty()) else { return Lang::Unspecified };
    let language = language.to_ascii_lowercase();
    let mut parts = language.split(['-', '_']);
    match parts.next() {
        Some("zh") => {
            if parts.any(|part| matches!(part, "hant" | "tw" | "hk" | "mo")) {
                Lang::ZhHant
            } else {
                Lang::ZhHans
            }
        }
        Some("ja") => Lang::Ja,
        Some("ko") => Lang::Ko,
        _ => Lang::Other,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScriptClass {
    Han,
    Kana,
    Hangul,
    Arabic,
    Hebrew,
    Thai,
    Devanagari,
    Bengali,
    Gurmukhi,
    Gujarati,
    Tamil,
    Telugu,
    Kannada,
    Malayalam,
    Sinhala,
    Khmer,
    Lao,
    Myanmar,
    Armenian,
    Georgian,
    Ethiopic,
    Tibetan,
    Symbols,
    Other,
}

pub(crate) fn script_class(ch: char) -> ScriptClass {
    let codepoint = ch as u32;
    match ch.script() {
        Script::Han | Script::Bopomofo => ScriptClass::Han,
        Script::Hiragana | Script::Katakana => ScriptClass::Kana,
        Script::Hangul => ScriptClass::Hangul,
        Script::Arabic | Script::Syriac | Script::Thaana => ScriptClass::Arabic,
        Script::Hebrew => ScriptClass::Hebrew,
        Script::Thai => ScriptClass::Thai,
        Script::Devanagari => ScriptClass::Devanagari,
        Script::Bengali => ScriptClass::Bengali,
        Script::Gurmukhi => ScriptClass::Gurmukhi,
        Script::Gujarati => ScriptClass::Gujarati,
        Script::Tamil => ScriptClass::Tamil,
        Script::Telugu => ScriptClass::Telugu,
        Script::Kannada => ScriptClass::Kannada,
        Script::Malayalam => ScriptClass::Malayalam,
        Script::Sinhala => ScriptClass::Sinhala,
        Script::Khmer => ScriptClass::Khmer,
        Script::Lao => ScriptClass::Lao,
        Script::Myanmar => ScriptClass::Myanmar,
        Script::Armenian => ScriptClass::Armenian,
        Script::Georgian => ScriptClass::Georgian,
        Script::Ethiopic => ScriptClass::Ethiopic,
        Script::Tibetan => ScriptClass::Tibetan,
        Script::Common | Script::Inherited => {
            if matches!(codepoint, 0x3000..=0x303f | 0x3200..=0x33ff | 0xff00..=0xffef) {
                ScriptClass::Han
            } else {
                ScriptClass::Symbols
            }
        }
        _ => ScriptClass::Other,
    }
}

type Names = &'static [&'static str];

/// The families a generic keyword maps to on `platform`, best first. They follow Chrome's default
/// font preferences (Times, Helvetica and Courier on macOS; Times New Roman, Arial and Consolas on
/// Windows; the common Linux families through fontconfig's usual choices).
pub(crate) fn generic_names(platform: FontPlatform, generic: GenericFontFamily) -> Names {
    use GenericFontFamily as G;
    match platform {
        FontPlatform::MacOs => match generic {
            G::SystemUi | G::UiSansSerif => &["sf pro", ".sf ns", ".applesystemuifont", "system font", "sfns", "helvetica neue", "helvetica"],
            G::UiRounded => &["sf pro rounded", "sfnsrounded", "helvetica neue"],
            G::UiSerif => &["new york", "newyork", "times"],
            G::UiMonospace => &["sf mono", "sfnsmono", "menlo"],
            G::SansSerif => &["helvetica", "arial"],
            G::Serif => &["times", "times new roman"],
            G::Monospace => &["courier", "menlo", "courier new"],
            G::Cursive => &["apple chancery", "snell roundhand"],
            G::Fantasy => &["papyrus", "impact"],
            G::Math => &["stix two math", "stixgeneral"],
            G::Fangsong => &[],
            G::Emoji => &[],
        },
        FontPlatform::Windows => match generic {
            G::SystemUi | G::UiSansSerif | G::UiRounded => &["segoe ui", "arial"],
            G::UiSerif | G::Serif => &["times new roman"],
            G::UiMonospace => &["cascadia mono", "consolas", "courier new"],
            G::SansSerif => &["arial", "segoe ui"],
            G::Monospace => &["consolas", "courier new"],
            G::Cursive => &["segoe script", "comic sans ms"],
            G::Fantasy => &["impact"],
            G::Math => &["cambria math"],
            G::Fangsong => &[],
            G::Emoji => &[],
        },
        FontPlatform::Linux => match generic {
            G::SystemUi | G::UiSansSerif | G::UiRounded => &["cantarell", "noto sans", "dejavu sans", "ubuntu", "liberation sans"],
            G::SansSerif => &["dejavu sans", "liberation sans", "noto sans", "arial", "helvetica"],
            G::UiSerif | G::Serif => &["dejavu serif", "liberation serif", "noto serif", "times new roman"],
            G::UiMonospace | G::Monospace => &["dejavu sans mono", "liberation mono", "noto sans mono", "courier new"],
            G::Cursive => &["z003", "urw chancery l", "comic sans ms"],
            G::Fantasy => &["impact"],
            G::Math => &["stix two math", "noto sans math"],
            G::Fangsong => &[],
            G::Emoji => &[],
        },
    }
}

/// Non-standard names pages use for the system UI font.
fn named_generic(key: &str) -> Option<GenericFontFamily> {
    (key == "-apple-system").then_some(GenericFontFamily::SystemUi)
}

/// Families that cover `ch` on `platform`, preferred first; the second list covers any script.
pub(crate) fn fallback_names(platform: FontPlatform, ch: char, language: Lang) -> [Names; 2] {
    use ScriptClass as S;
    let class = script_class(ch);
    let language = match (class, language) {
        (S::Kana, Lang::Unspecified | Lang::Other) => Lang::Ja,
        (_, language) => language,
    };
    match platform {
        FontPlatform::MacOs => {
            let preferred: Names = match class {
                S::Han | S::Kana => match language {
                    Lang::Ja => &["hiragino sans", "hiragino kaku gothic pron", "ヒラギノ角ゴシック", "hiragino mincho pron", "pingfang sc", ".pingfang sc", "hiragino sans gb"],
                    Lang::ZhHant => &["pingfang tc", ".pingfang tc", "pingfang hk", "heiti tc", "stheiti", "hiragino sans gb"],
                    Lang::Ko => &["apple sd gothic neo", "applegothic", "pingfang sc", ".pingfang sc", "hiragino sans gb"],
                    _ => &["pingfang sc", ".pingfang sc", "hiragino sans gb", "stheiti", "heiti sc", "songti sc", "hiragino sans", "hiragino kaku gothic pron", "ヒラギノ角ゴシック"],
                },
                S::Hangul => &["apple sd gothic neo", "applegothic", "nanumgothic"],
                S::Arabic => &["geeza pro", "sf arabic", "sfarabic", "al bayan", "baghdad", "damascus", "arial unicode ms"],
                S::Hebrew => &["arial hebrew", "sf hebrew", "sfhebrew", "new peninim mt", "arial"],
                S::Thai => &["thonburi", "thonburiui", "ayuthaya", "sukhumvit set", "krungthep"],
                S::Devanagari => &["kohinoor devanagari", "devanagari sangam mn", "itf devanagari", "devanagari mt"],
                S::Bengali => &["kohinoor bangla", "bangla sangam mn", "bangla mn"],
                S::Gurmukhi => &["gurmukhi sangam mn", "gurmukhi mn", "gurmukhi mt"],
                S::Gujarati => &["kohinoor gujarati", "gujarati sangam mn", "gujarati mt"],
                S::Tamil => &["tamil sangam mn", "tamil mn", "inaimathi"],
                S::Telugu => &["kohinoor telugu", "telugu sangam mn", "telugu mn"],
                S::Kannada => &["kannada sangam mn", "kannada mn", "noto sans kannada"],
                S::Malayalam => &["malayalam sangam mn", "malayalam mn"],
                S::Sinhala => &["sinhala sangam mn", "sinhala mn"],
                S::Khmer => &["khmer sangam mn", "khmer mn"],
                S::Lao => &["lao sangam mn", "lao mn"],
                S::Myanmar => &["myanmar sangam mn", "myanmar mn", "noto sans myanmar"],
                S::Armenian => &["mshtakan", "sf armenian", "noto sans armenian"],
                S::Georgian => &["sf georgian", "arial unicode ms"],
                S::Ethiopic => &["kefa"],
                S::Tibetan => &["kailasa"],
                S::Symbols => &["apple symbols", "menlo", "stix two math", "stixgeneral", "zapf dingbats", "symbol", "lucida grande"],
                S::Other => &["lucida grande", "helvetica neue", "arial unicode ms"],
            };
            [preferred, &["arial unicode ms", "lucida grande", "apple symbols", "menlo"]]
        }
        FontPlatform::Windows => {
            let preferred: Names = match class {
                S::Han | S::Kana => match language {
                    Lang::Ja => &["yu gothic", "meiryo", "ms gothic", "microsoft yahei"],
                    Lang::ZhHant => &["microsoft jhenghei", "pmingliu", "microsoft yahei"],
                    Lang::Ko => &["malgun gothic", "gulim", "microsoft yahei"],
                    _ => &["microsoft yahei", "simsun", "dengxian", "microsoft jhenghei", "yu gothic"],
                },
                S::Hangul => &["malgun gothic", "gulim"],
                S::Arabic => &["segoe ui", "tahoma", "arial"],
                S::Hebrew => &["segoe ui", "arial", "david"],
                S::Thai => &["leelawadee ui", "tahoma"],
                S::Devanagari | S::Bengali | S::Gurmukhi | S::Gujarati | S::Tamil | S::Telugu | S::Kannada | S::Malayalam | S::Sinhala => {
                    &["nirmala ui", "mangal"]
                }
                S::Khmer => &["leelawadee ui", "khmer ui"],
                S::Lao => &["leelawadee ui", "lao ui"],
                S::Myanmar => &["myanmar text"],
                S::Armenian | S::Georgian | S::Ethiopic => &["segoe ui", "sylfaen", "nyala"],
                S::Tibetan => &["microsoft himalaya"],
                S::Symbols => &["segoe ui symbol", "cambria math", "segoe ui", "arial unicode ms"],
                S::Other => &["segoe ui", "arial unicode ms", "arial"],
            };
            [preferred, &["segoe ui", "arial unicode ms", "segoe ui symbol", "microsoft yahei"]]
        }
        FontPlatform::Linux => {
            let preferred: Names = match class {
                S::Han | S::Kana => match language {
                    Lang::Ja => &["noto sans cjk jp", "source han sans jp", "ipagothic", "takao gothic", "noto sans cjk sc"],
                    Lang::ZhHant => &["noto sans cjk tc", "source han sans tc", "wenquanyi micro hei", "noto sans cjk sc"],
                    Lang::Ko => &["noto sans cjk kr", "source han sans kr", "nanumgothic", "un dotum", "noto sans cjk sc"],
                    _ => &["noto sans cjk sc", "source han sans sc", "wenquanyi micro hei", "wenquanyi zen hei", "droid sans fallback", "noto sans cjk jp"],
                },
                S::Hangul => &["noto sans cjk kr", "nanumgothic", "un dotum", "noto sans cjk sc"],
                S::Arabic => &["noto sans arabic", "noto naskh arabic", "dejavu sans", "amiri"],
                S::Hebrew => &["noto sans hebrew", "dejavu sans", "liberation sans"],
                S::Thai => &["noto sans thai", "loma", "garuda"],
                S::Devanagari => &["noto sans devanagari", "lohit devanagari"],
                S::Bengali => &["noto sans bengali", "lohit bengali"],
                S::Tamil => &["noto sans tamil", "lohit tamil"],
                S::Symbols => &["dejavu sans", "noto sans symbols", "noto sans symbols2", "noto sans math"],
                S::Gurmukhi | S::Gujarati | S::Telugu | S::Kannada | S::Malayalam | S::Sinhala | S::Khmer | S::Lao | S::Myanmar | S::Armenian | S::Georgian | S::Ethiopic | S::Tibetan | S::Other => {
                    &["dejavu sans", "noto sans"]
                }
            };
            [preferred, &["dejavu sans", "noto sans", "liberation sans", "freesans", "unifont"]]
        }
    }
}

/// The fallback face for `ch`: the first family of the script's preference list, then of the
/// any-script list, whose style matching `spec` has a glyph for it, then the broadest installed
/// faces (`scan`). `covers(face, ch)` reports glyph coverage.
pub(crate) fn pick_fallback(
    index: &SystemFontIndex,
    platform: FontPlatform,
    ch: char,
    language: Lang,
    spec: &FontSpec,
    scan: &[u32],
    covers: &mut dyn FnMut(usize, char) -> bool,
) -> Option<usize> {
    for names in fallback_names(platform, ch, language) {
        for name in names {
            if let Some(face) = index.best_match(name, spec) {
                if covers(face, ch) {
                    return Some(face);
                }
            }
        }
    }
    scan.iter().map(|&face| face as usize).find(|&face| covers(face, ch))
}

/// A system face once loaded, in the shape `FontSet` registers faces.
pub(crate) struct SystemFace {
    pub font: RegisteredFont,
    pub descriptors: RegisteredFontDescriptors,
}

struct SystemFonts {
    index: SystemFontIndex,
    platform: FontPlatform,
    faces: Box<[OnceLock<Option<SystemFace>>]>,
    loaded: Mutex<Vec<u32>>,
    by_id: Mutex<HashMap<u64, u32>>,
    loaded_bytes: AtomicUsize,
    cmaps: Mutex<Vec<(u32, Option<Arc<[u8]>>)>>,
    scan: OnceLock<Vec<u32>>,
    fallback: Mutex<HashMap<(char, Lang, u8), Option<u32>>>,
}

impl SystemFonts {
    fn new(index: SystemFontIndex, platform: FontPlatform) -> Self {
        let faces = (0..index.entries.len()).map(|_| OnceLock::new()).collect();
        Self {
            index,
            platform,
            faces,
            loaded: Mutex::new(Vec::new()),
            by_id: Mutex::new(HashMap::new()),
            loaded_bytes: AtomicUsize::new(0),
            cmaps: Mutex::new(Vec::new()),
            scan: OnceLock::new(),
            fallback: Mutex::new(HashMap::new()),
        }
    }

    /// Loads the face on first use and returns its tagged index; None when it cannot be read or
    /// the bounded set of loaded faces is full.
    fn ensure(&self, entry: usize) -> Option<usize> {
        let slot = self.faces.get(entry)?.get_or_init(|| self.load(entry));
        slot.as_ref().map(|_| SYSTEM_FACE_BASE + entry)
    }

    fn load(&self, position: usize) -> Option<SystemFace> {
        let entry = &self.index.entries[position];
        if lock(&self.loaded).len() >= MAX_LOADED_FACES {
            return None;
        }
        let size = usize::try_from(entry.face_length).unwrap_or(usize::MAX);
        self.loaded_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| used.checked_add(size).filter(|total| *total <= MAX_LOADED_BYTES))
            .ok()?;
        let face = self.open(position, entry);
        if face.is_none() {
            self.loaded_bytes.fetch_sub(size, Ordering::AcqRel);
        }
        face
    }

    fn open(&self, position: usize, entry: &SystemFontEntry) -> Option<SystemFace> {
        let bytes = read_entry_bytes(entry).ok()?;
        let mut face = FontFace::new_with_limit(Arc::from(bytes), MAX_SYSTEM_FONT_BYTES).ok()?;
        if self.platform == FontPlatform::MacOs && matches!(&*entry.family.to_lowercase(), "times" | "helvetica" | "courier") {
            face = face.with_classic_mac_ascent();
        }
        let face = Arc::new(face);
        lock(&self.by_id).insert(face.id(), position as u32);
        lock(&self.loaded).push(position as u32);
        Some(SystemFace {
            font: RegisteredFont {
                family: entry.family.clone(),
                weight: entry.weight.clamp(1, 1000),
                style: entry.style,
                stretch: entry.stretch_range[0],
                face,
            },
            descriptors: RegisteredFontDescriptors {
                oblique_range: None,
                weight_range: entry.weight_range,
                stretch_range: entry.stretch_range,
                size_adjust: 1.0,
                feature_settings: None,
                family_scope: None,
                feature_values: None,
            },
        })
    }

    fn cmap_table(&self, entry: usize) -> Option<Arc<[u8]>> {
        let mut cache = lock(&self.cmaps);
        if let Some(position) = cache.iter().position(|(cached, _)| *cached as usize == entry) {
            let hit = cache.remove(position);
            let table = hit.1.clone();
            cache.push(hit);
            return table;
        }
        drop(cache);
        let table = self.index.entries[entry].cmap.and_then(|(offset, length)| {
            if u64::from(length) > MAX_TABLE_BYTES {
                return None;
            }
            let mut file = std::fs::File::open(&self.index.entries[entry].path).ok()?;
            read_at(&mut file, offset, length as usize).ok().map(Arc::<[u8]>::from)
        });
        let mut cache = lock(&self.cmaps);
        if let Some((_, cached)) = cache.iter().find(|(cached, _)| *cached as usize == entry) {
            return cached.clone();
        }
        if cache.len() >= MAX_CMAP_CACHE {
            cache.remove(0);
        }
        cache.push((entry as u32, table.clone()));
        table
    }

    fn covers(&self, entry: usize, ch: char) -> bool {
        let Some(table) = self.cmap_table(entry) else { return false };
        ttf_parser::cmap::Table::parse(&table).is_some_and(|cmap| {
            cmap.subtables
                .into_iter()
                .any(|subtable| subtable.is_unicode() && subtable.glyph_index(ch as u32).is_some_and(|glyph| glyph.0 != 0))
        })
    }

    fn named(&self, name: &str, spec: &FontSpec) -> Option<usize> {
        let key = name.to_lowercase();
        // Names starting with '.' are the platform's private UI fonts; pages reach them only
        // through the generic families.
        if key.starts_with('.') {
            return None;
        }
        if let Some(generic) = named_generic(&key) {
            return self.generic(generic, spec);
        }
        self.ensure(self.index.best_match(&key, spec)?)
    }

    fn generic(&self, generic: GenericFontFamily, spec: &FontSpec) -> Option<usize> {
        generic_names(self.platform, generic)
            .iter()
            .filter_map(|name| self.index.best_match(name, spec))
            .find_map(|entry| self.ensure(entry))
    }

    fn fallback(&self, ch: char, language: Option<&str>, spec: &FontSpec) -> Option<usize> {
        let language = lang_class(language);
        let style = u8::from(spec.weight >= 600) | (u8::from(!matches!(spec.style, FontStyle::Normal)) << 1);
        let key = (ch, language, style);
        let cached = lock(&self.fallback).get(&key).copied();
        let entry = match cached {
            Some(entry) => entry,
            None => {
                let scan = self.scan.get_or_init(|| self.index.scan_list());
                let entry = pick_fallback(&self.index, self.platform, ch, language, spec, scan, &mut |face, ch| self.covers(face, ch))
                    .and_then(|entry| self.ensure(entry).map(|_| entry as u32));
                let mut cache = lock(&self.fallback);
                if cache.len() >= MAX_FALLBACK_CACHE {
                    cache.clear();
                }
                cache.insert(key, entry);
                entry
            }
        };
        entry.map(|entry| SYSTEM_FACE_BASE + entry as usize)
    }
}

fn read_entry_bytes(entry: &SystemFontEntry) -> io::Result<Vec<u8>> {
    if !entry.collection {
        if entry.file_length > MAX_SYSTEM_FONT_BYTES as u64 {
            return Err(invalid("font too large"));
        }
        return std::fs::read(&entry.path);
    }
    let mut file = std::fs::File::open(&entry.path)?;
    let face = read_face(&mut file, entry.face_offset, entry.file_length)?;
    assemble_standalone(&mut file, &face)
}

static STATE: OnceLock<SystemFonts> = OnceLock::new();

fn state() -> &'static SystemFonts {
    STATE.get_or_init(|| {
        let files = lumen_os::fonts::system_font_files();
        SystemFonts::new(SystemFontIndex::scan(&files), FontPlatform::current())
    })
}

/// The installed face for an author-written family name, as a tagged face index.
pub(crate) fn named(name: &str, spec: &FontSpec) -> Option<usize> {
    state().named(name, spec)
}

pub(crate) fn generic(generic: GenericFontFamily, spec: &FontSpec) -> Option<usize> {
    state().generic(generic, spec)
}

pub(crate) fn fallback(ch: char, language: Option<&str>, spec: &FontSpec) -> Option<usize> {
    state().fallback(ch, language, spec)
}

/// A face returned by [`named`], [`generic`] or [`fallback`].
pub(crate) fn face(index: usize) -> &'static SystemFace {
    state().faces[index - SYSTEM_FACE_BASE]
        .get()
        .and_then(Option::as_ref)
        .expect("system face indices are handed out loaded")
}

pub(crate) fn by_identity(identity: u64) -> Option<&'static Arc<FontFace>> {
    let state = STATE.get()?;
    let known = lock(&state.by_id).get(&identity).copied();
    let position = match known {
        Some(position) => position,
        None => {
            let loaded = lock(&state.loaded);
            let found = loaded.iter().copied().find(|&position| {
                state.faces[position as usize].get().and_then(Option::as_ref).is_some_and(|loaded| {
                    let face = &loaded.font.face;
                    face.is_invisible_key(identity) || face.is_oblique_key(identity) || face.is_variation_key(identity)
                })
            });
            drop(loaded);
            found?
        }
    };
    state.faces[position as usize].get()?.as_ref().map(|loaded| &loaded.font.face)
}

#[cfg(test)]
mod tests;
