use super::*;
use std::io::Cursor;

fn entry(family: &str, keys: &[&str], weight: u16, style: FontStyle, cmap_length: u32) -> SystemFontEntry {
    SystemFontEntry {
        family: Arc::from(family),
        keys: keys.iter().map(|key| Arc::from(*key)).collect(),
        weight,
        weight_range: [weight, weight],
        stretch_range: [100.0, 100.0],
        style,
        path: Arc::from(Path::new("/nonexistent/font.ttf")),
        collection: false,
        face_offset: 0,
        file_length: 0,
        face_length: 0,
        cmap: Some((0, cmap_length)),
    }
}

fn spec(weight: u16, style: FontStyle) -> FontSpec {
    FontSpec { weight, style, ..FontSpec::default() }
}

fn scan(bytes: &[u8], name: &str) -> Vec<SystemFontEntry> {
    scan_reader(&mut Cursor::new(bytes), &Arc::from(Path::new(name)), bytes.len() as u64)
}

/// A TrueType collection whose members are copies of `fonts`.
fn collection(fonts: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![0u8; 12 + 4 * fonts.len()];
    out[..4].copy_from_slice(b"ttcf");
    out[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
    out[8..12].copy_from_slice(&(fonts.len() as u32).to_be_bytes());
    for (member, font) in fonts.iter().enumerate() {
        let start = out.len();
        out[12 + 4 * member..16 + 4 * member].copy_from_slice(&(start as u32).to_be_bytes());
        let count = usize::from(be16(font, 4).unwrap());
        let mut directory = font[..12 + 16 * count].to_vec();
        let mut data = Vec::new();
        for table in 0..count {
            let record = 12 + table * 16;
            let offset = be32(font, record + 8).unwrap() as usize;
            let length = be32(font, record + 12).unwrap() as usize;
            let rebased = start + directory.len() + data.len();
            directory[record + 8..record + 12].copy_from_slice(&(rebased as u32).to_be_bytes());
            data.extend_from_slice(&font[offset..offset + length]);
            data.resize(data.len().next_multiple_of(4), 0);
        }
        out.extend_from_slice(&directory);
        out.extend_from_slice(&data);
    }
    out
}

fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    let path = std::env::temp_dir().join(format!("lumen-system-fonts-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

#[cfg(feature = "bundled-fonts")]
mod bundled {
    use super::*;
    use crate::{DEFAULT_FONT_BYTES, TEST_FONT_BOLD_BYTES, TEST_FONT_BYTES};

    #[test]
    fn name_os2_and_cmap_of_plain_fonts() {
        let regular = scan(TEST_FONT_BYTES, "/fonts/LiberationSans-Regular.ttf");
        assert_eq!(regular.len(), 1);
        assert_eq!(&*regular[0].family, "Liberation Sans");
        assert!(regular[0].keys.iter().any(|key| &**key == "liberation sans"));
        assert!(regular[0].keys.iter().any(|key| &**key == "liberationsans-regular"));
        assert_eq!((regular[0].weight, regular[0].style, regular[0].collection), (400, FontStyle::Normal, false));
        assert!(regular[0].cmap.is_some());

        let bold = scan(TEST_FONT_BOLD_BYTES, "bold.ttf");
        assert_eq!((bold[0].weight, bold[0].weight_range), (700, [700, 700]));

        let mono = scan(DEFAULT_FONT_BYTES, "mono.ttf");
        assert_eq!(&*mono[0].family, "Inconsolata");
    }

    #[test]
    fn family_names_prefer_english_and_keep_every_spelling() {
        let mut reader = Cursor::new(TEST_FONT_BYTES);
        let face = read_faces(&mut reader, TEST_FONT_BYTES.len() as u64).unwrap().remove(0);
        let table = table_bytes(&mut reader, &face, b"name").unwrap();
        let (display, keys) = family_names(&table).unwrap();
        assert_eq!(display, "Liberation Sans");
        assert!(keys.contains(&"liberation sans".to_owned()));
        assert!(family_names(&table[..6]).is_none());
    }

    #[test]
    fn collections_index_each_member_and_extract_standalone_faces() {
        let bytes = collection(&[TEST_FONT_BYTES, TEST_FONT_BOLD_BYTES]);
        let entries = scan(&bytes, "/fonts/Pair.ttc");
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.collection));
        assert_eq!(entries.iter().map(|entry| entry.weight).collect::<Vec<_>>(), [400, 700]);
        assert_ne!(entries[0].face_offset, entries[1].face_offset);

        let mut reader = Cursor::new(&bytes);
        let face = read_face(&mut reader, entries[1].face_offset, bytes.len() as u64).unwrap();
        let standalone = assemble_standalone(&mut reader, &face).unwrap();
        assert!(standalone.len() < bytes.len());
        assert_eq!(entries[1].face_length, standalone.len() as u64);
        let parsed = ttf_parser::Face::parse(&standalone, 0).unwrap();
        assert_eq!(parsed.weight().to_number(), 700);
        assert!(FontFace::new(Arc::from(standalone)).is_ok());
    }

    #[test]
    fn unreadable_files_yield_no_faces() {
        assert!(scan(&[1, 2, 3], "x.ttf").is_empty());
        assert!(scan(&TEST_FONT_BYTES[..300], "truncated.ttf").is_empty());
        let mut not_a_font = TEST_FONT_BYTES.to_vec();
        not_a_font[..4].copy_from_slice(b"wOFF");
        assert!(scan(&not_a_font, "woff.ttf").is_empty());
    }

    #[test]
    fn faces_load_lazily_from_plain_files_and_collections() {
        let plain = temp_file("plain.ttf", TEST_FONT_BYTES);
        let pair = temp_file("pair.ttc", &collection(&[TEST_FONT_BYTES, TEST_FONT_BOLD_BYTES]));
        let fonts = SystemFonts::new(SystemFontIndex::scan(&[plain.clone(), pair.clone()]), FontPlatform::Linux);
        assert_eq!(fonts.faces.len(), 3);
        assert!(fonts.faces.iter().all(|slot| slot.get().is_none()));

        let bold = fonts.named("LIBERATION SANS", &spec(700, FontStyle::Normal)).unwrap();
        assert_eq!(bold, SYSTEM_FACE_BASE + 2);
        assert_eq!(fonts.faces[2].get().unwrap().as_ref().unwrap().font.weight, 700);
        assert!(fonts.faces[0].get().is_none() && fonts.faces[1].get().is_none());
        assert_eq!(fonts.loaded.lock().unwrap().as_slice(), [2]);

        assert!(fonts.covers(0, 'A'));
        assert!(!fonts.covers(0, '中'));
        assert_eq!(fonts.fallback('中', None, &FontSpec::default()), None);
        assert_eq!(fonts.fallback('A', Some("en"), &FontSpec::default()), Some(SYSTEM_FACE_BASE));
        assert_eq!(fonts.named("no such family", &FontSpec::default()), None);

        let _ = std::fs::remove_file(plain);
        let _ = std::fs::remove_file(pair);
    }
}

#[test]
fn css_matching_picks_weight_and_style_within_a_family() {
    let mut narrow = entry("Arial Narrow", &["arial", "arial narrow"], 400, FontStyle::Normal, 10);
    narrow.stretch_range = [75.0, 75.0];
    let index = SystemFontIndex::from_entries(vec![
        entry("Verdana", &["verdana"], 400, FontStyle::Normal, 10),
        entry("Verdana", &["verdana"], 700, FontStyle::Normal, 10),
        entry("Verdana", &["verdana"], 400, FontStyle::Italic, 10),
        entry("Verdana", &["verdana"], 700, FontStyle::Italic, 10),
        narrow,
        entry("Arial", &["arial"], 400, FontStyle::Normal, 10),
    ]);
    assert_eq!(index.best_match("verdana", &spec(400, FontStyle::Normal)), Some(0));
    assert_eq!(index.best_match("verdana", &spec(600, FontStyle::Normal)), Some(1));
    assert_eq!(index.best_match("verdana", &spec(400, FontStyle::Italic)), Some(2));
    assert_eq!(index.best_match("verdana", &spec(900, FontStyle::Oblique)), Some(3));
    assert_eq!(index.best_match("arial", &FontSpec::default()), Some(5));
    assert_eq!(index.best_match("arial narrow", &FontSpec::default()), Some(4));
    assert_eq!(index.best_match("geneva", &FontSpec::default()), None);
}

#[test]
fn variable_faces_match_across_their_axis_range() {
    let mut variable = entry("SF", &["sf"], 400, FontStyle::Normal, 10);
    variable.weight_range = [100, 900];
    let index = SystemFontIndex::from_entries(vec![variable, entry("SF", &["sf"], 400, FontStyle::Italic, 10)]);
    assert_eq!(index.best_match("sf", &spec(650, FontStyle::Normal)), Some(0));
}

#[test]
fn generic_families_map_to_platform_defaults() {
    use GenericFontFamily as G;
    assert_eq!(generic_names(FontPlatform::MacOs, G::SansSerif)[0], "helvetica");
    assert_eq!(generic_names(FontPlatform::MacOs, G::Serif)[0], "times");
    assert_eq!(generic_names(FontPlatform::MacOs, G::Monospace)[0], "courier");
    assert!(generic_names(FontPlatform::MacOs, G::SystemUi).contains(&"sfns"));
    assert_eq!(generic_names(FontPlatform::Windows, G::SansSerif)[0], "arial");
    assert_eq!(generic_names(FontPlatform::Windows, G::Monospace)[0], "consolas");
    assert_eq!(generic_names(FontPlatform::Windows, G::SystemUi)[0], "segoe ui");
    assert_eq!(generic_names(FontPlatform::Linux, G::Monospace)[0], "dejavu sans mono");
    for platform in [FontPlatform::MacOs, FontPlatform::Windows, FontPlatform::Linux] {
        for generic in [G::Serif, G::SansSerif, G::Monospace, G::Cursive, G::Fantasy, G::SystemUi, G::UiSerif, G::UiSansSerif, G::UiMonospace, G::UiRounded, G::Math] {
            assert!(!generic_names(platform, generic).is_empty(), "{platform:?} {generic:?}");
        }
        assert!(generic_names(platform, G::Emoji).is_empty());
        assert!(generic_names(platform, G::Fangsong).is_empty());
    }
    assert_eq!(named_generic("-apple-system"), Some(G::SystemUi));
    assert_eq!(named_generic("blinkmacsystemfont"), None);
    assert_eq!(named_generic("helvetica"), None);
}

#[test]
fn languages_and_scripts_are_classified() {
    assert_eq!(lang_class(None), Lang::Unspecified);
    assert_eq!(lang_class(Some("")), Lang::Unspecified);
    assert_eq!(lang_class(Some("zh")), Lang::ZhHans);
    assert_eq!(lang_class(Some("zh-CN")), Lang::ZhHans);
    assert_eq!(lang_class(Some("zh-Hant")), Lang::ZhHant);
    assert_eq!(lang_class(Some("ZH_tw")), Lang::ZhHant);
    assert_eq!(lang_class(Some("zh-HK")), Lang::ZhHant);
    assert_eq!(lang_class(Some("ja-JP")), Lang::Ja);
    assert_eq!(lang_class(Some("ko")), Lang::Ko);
    assert_eq!(lang_class(Some("en-US")), Lang::Other);
    assert_eq!(script_class('中'), ScriptClass::Han);
    assert_eq!(script_class('。'), ScriptClass::Han);
    assert_eq!(script_class('あ'), ScriptClass::Kana);
    assert_eq!(script_class('한'), ScriptClass::Hangul);
    assert_eq!(script_class('م'), ScriptClass::Arabic);
    assert_eq!(script_class('ש'), ScriptClass::Hebrew);
    assert_eq!(script_class('ก'), ScriptClass::Thai);
    assert_eq!(script_class('अ'), ScriptClass::Devanagari);
    assert_eq!(script_class('→'), ScriptClass::Symbols);
    assert_eq!(script_class('a'), ScriptClass::Other);
}

fn mac_index() -> SystemFontIndex {
    let names = [
        "PingFang SC",
        "PingFang TC",
        "Hiragino Sans",
        "Hiragino Sans GB",
        "Apple SD Gothic Neo",
        "Geeza Pro",
        "Thonburi",
        "Apple Symbols",
        "Arial Unicode MS",
    ];
    SystemFontIndex::from_entries(
        names
            .iter()
            .enumerate()
            .map(|(position, name)| entry(name, &[&name.to_lowercase()], 400, FontStyle::Normal, 100 + position as u32))
            .collect(),
    )
}

fn pick(index: &SystemFontIndex, ch: char, language: Lang, covered: &dyn Fn(&str, char) -> bool) -> Option<String> {
    let scan = index.scan_list();
    pick_fallback(index, FontPlatform::MacOs, ch, language, &FontSpec::default(), &scan, &mut |face, ch| {
        covered(&index.entries[face].family, ch)
    })
    .map(|face| index.entries[face].family.to_string())
}

#[test]
fn fallback_follows_the_script_preference_list() {
    let index = mac_index();
    let everything = |_: &str, _: char| true;
    assert_eq!(pick(&index, '中', Lang::Unspecified, &everything).as_deref(), Some("PingFang SC"));
    assert_eq!(pick(&index, '中', Lang::ZhHans, &everything).as_deref(), Some("PingFang SC"));
    assert_eq!(pick(&index, '中', Lang::ZhHant, &everything).as_deref(), Some("PingFang TC"));
    assert_eq!(pick(&index, '中', Lang::Ja, &everything).as_deref(), Some("Hiragino Sans"));
    assert_eq!(pick(&index, 'あ', Lang::Unspecified, &everything).as_deref(), Some("Hiragino Sans"));
    assert_eq!(pick(&index, '한', Lang::Unspecified, &everything).as_deref(), Some("Apple SD Gothic Neo"));
    assert_eq!(pick(&index, 'م', Lang::Unspecified, &everything).as_deref(), Some("Geeza Pro"));
    assert_eq!(pick(&index, 'ก', Lang::Unspecified, &everything).as_deref(), Some("Thonburi"));
    assert_eq!(pick(&index, '→', Lang::Unspecified, &everything).as_deref(), Some("Apple Symbols"));
}

#[test]
fn fallback_skips_faces_without_the_glyph_then_scans_the_broadest() {
    let index = mac_index();
    let rare = |family: &str, _: char| family == "Hiragino Sans GB";
    assert_eq!(pick(&index, '龘', Lang::Unspecified, &rare).as_deref(), Some("Hiragino Sans GB"));
    let only_widest = |family: &str, _: char| family == "Arial Unicode MS";
    assert_eq!(pick(&index, 'ꚙ', Lang::Unspecified, &only_widest).as_deref(), Some("Arial Unicode MS"));
    assert_eq!(pick(&index, 'ꚙ', Lang::Unspecified, &|_, _| false), None);
}

#[test]
fn fallback_respects_the_style_of_the_run() {
    let index = SystemFontIndex::from_entries(vec![
        entry("PingFang SC", &["pingfang sc"], 400, FontStyle::Normal, 10),
        entry("PingFang SC", &["pingfang sc"], 700, FontStyle::Normal, 10),
    ]);
    let picked = pick_fallback(&index, FontPlatform::MacOs, '中', Lang::Unspecified, &spec(700, FontStyle::Normal), &[], &mut |_, _| true);
    assert_eq!(picked, Some(1));
}

#[test]
fn scan_list_orders_by_character_map_size_and_skips_last_resort() {
    let index = SystemFontIndex::from_entries(vec![
        entry("Small", &["small"], 400, FontStyle::Normal, 10),
        entry("Last Resort", &["last resort"], 400, FontStyle::Normal, 9999),
        entry("Large", &["large"], 400, FontStyle::Normal, 500),
        entry("Medium", &["medium"], 400, FontStyle::Normal, 50),
    ]);
    assert_eq!(index.scan_list(), [2, 3, 0]);
}

#[test]
fn an_empty_index_resolves_nothing() {
    let fonts = SystemFonts::new(SystemFontIndex::from_entries(Vec::new()), FontPlatform::MacOs);
    let spec = FontSpec::default();
    assert_eq!(fonts.named("Helvetica", &spec), None);
    assert_eq!(fonts.named("-apple-system", &spec), None);
    assert_eq!(fonts.generic(GenericFontFamily::SansSerif, &spec), None);
    assert_eq!(fonts.fallback('中', Some("zh-CN"), &spec), None);
}

fn tiny_sfnt() -> Vec<u8> {
    let mut font = vec![0u8; 28];
    font[..4].copy_from_slice(&[0, 1, 0, 0]);
    font[4..6].copy_from_slice(&1u16.to_be_bytes());
    font[12..16].copy_from_slice(b"glyf");
    font[20..24].copy_from_slice(&44u32.to_be_bytes());
    font
}

#[test]
fn a_collection_count_beyond_the_file_keeps_the_faces_that_fit() {
    let sfnt = tiny_sfnt();
    let mut bytes = vec![0u8; 16];
    bytes[..4].copy_from_slice(b"ttcf");
    bytes[8..12].copy_from_slice(&512u32.to_be_bytes());
    bytes[12..16].copy_from_slice(&16u32.to_be_bytes());
    bytes.extend_from_slice(&sfnt);
    let faces = read_faces(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap();
    assert_eq!(faces.len(), 1);
    assert_eq!(faces[0].offset, 16);
}

#[test]
fn the_cmap_cache_holds_the_scan_list_and_never_repeats_a_face() {
    assert!(MAX_CMAP_CACHE >= SCAN_LIST_LEN + 2 * 16);
    let entries = (0..MAX_CMAP_CACHE + 20)
        .map(|position| entry("F", &[&format!("f{position}")], 400, FontStyle::Normal, 10))
        .collect();
    let fonts = SystemFonts::new(SystemFontIndex::from_entries(entries), FontPlatform::Linux);
    for _ in 0..2 {
        for position in 0..MAX_CMAP_CACHE {
            fonts.cmap_table(position);
        }
    }
    assert_eq!(lock(&fonts.cmaps).len(), MAX_CMAP_CACHE);

    let fonts = SystemFonts::new(SystemFontIndex::from_entries(vec![entry("F", &["f"], 400, FontStyle::Normal, 10)]), FontPlatform::Linux);
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| fonts.cmap_table(0));
        }
    });
    assert_eq!(lock(&fonts.cmaps).len(), 1);
}

#[test]
fn the_loaded_byte_budget_is_reserved_and_released_atomically() {
    let mut huge = entry("Huge", &["huge"], 400, FontStyle::Normal, 10);
    huge.face_length = MAX_LOADED_BYTES as u64 + 1;
    let mut fits = entry("Fits", &["fits"], 400, FontStyle::Normal, 10);
    fits.face_length = MAX_LOADED_BYTES as u64;
    let fonts = SystemFonts::new(SystemFontIndex::from_entries(vec![huge, fits]), FontPlatform::Linux);

    assert!(fonts.load(0).is_none());
    assert_eq!(fonts.loaded_bytes.load(Ordering::Relaxed), 0);
    assert!(fonts.load(1).is_none());
    assert_eq!(fonts.loaded_bytes.load(Ordering::Relaxed), 0);

    fonts.loaded_bytes.store(1, Ordering::Relaxed);
    assert!(fonts.load(1).is_none());
    assert_eq!(fonts.loaded_bytes.load(Ordering::Relaxed), 1);
}

#[test]
fn private_platform_names_are_not_reachable_by_name() {
    let fonts = SystemFonts::new(SystemFontIndex::from_entries(vec![entry(".SF NS", &[".sf ns", "sfns"], 400, FontStyle::Normal, 4)]), FontPlatform::MacOs);
    assert_eq!(fonts.named(".SF NS", &spec(400, FontStyle::Normal)), None);
}
