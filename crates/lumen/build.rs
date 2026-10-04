use std::collections::BTreeSet;

fn main() {
    println!("cargo:rerun-if-env-changed=LUMEN_LOCALES");
    println!("cargo:rustc-check-cfg=cfg(lumen_locale, values(any()))");
    println!("cargo:rustc-check-cfg=cfg(lumen_all_locales)");
    let raw = std::env::var("LUMEN_LOCALES").ok();
    let mut languages = BTreeSet::from(["en".to_owned()]);
    if let Some(raw) = &raw {
        for locale in raw.split(',') {
            let locale = locale.trim();
            assert!(
                !locale.is_empty()
                    && locale.split('-').all(|part| !part.is_empty()
                        && part.len() <= 8
                        && part.bytes().all(|byte| byte.is_ascii_alphanumeric())),
                "LUMEN_LOCALES must be comma-separated BCP47 locale tags"
            );
            let language = locale.split('-').next().unwrap();
            assert!(
                (2..=8).contains(&language.len())
                    && language.bytes().all(|byte| byte.is_ascii_alphabetic()),
                "invalid LUMEN_LOCALES language"
            );
            languages.insert(language.to_ascii_lowercase());
        }
    } else {
        println!("cargo:rustc-cfg=lumen_all_locales");
    }
    for language in &languages {
        println!("cargo:rustc-cfg=lumen_locale=\"{language}\"");
    }
    let selection = if raw.is_none() {
        "None".to_owned()
    } else {
        format!("Some(&{:?})", languages.into_iter().collect::<Vec<_>>())
    };
    std::fs::write(
        std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("lumen_locales.rs"),
        format!("const SELECTED_LANGUAGES: Option<&[&str]> = {selection};\n"),
    )
    .unwrap();
}
