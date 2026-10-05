//! Valid format-7 envelope seed for the precompiled fuzz target.
use lumen_common::aot::{self, Language, Section};

fn main() -> Result<(), String> {
    let path = std::env::args().nth(1).ok_or("expected output path")?;
    let got = aot::got::encode(&[aot::got::Reloc {
        slot: 0,
        kind: aot::got::Kind::Helper,
        index: 0,
    }])
    .map_err(str::to_owned)?;
    let data = aot::native_data::encode(
        &[aot::native_data::FunctionEntry { offset: 0, len: 4 }],
        b"seed",
        4,
    )
    .map_err(str::to_owned)?;
    let blob = aot::encode_native(
        Language::JavaScript,
        1,
        aot::version_bytes("seed"),
        &[
            Section {
                kind: aot::SEC_NATIVE_CODE,
                flags: 0,
                data: b"\0\0\0\0",
            },
            Section {
                kind: aot::SEC_NATIVE_DATA,
                flags: 0,
                data: &data,
            },
            Section {
                kind: aot::SEC_NATIVE_GOT_RELOCS,
                flags: 0,
                data: &got,
            },
        ],
    )
    .map_err(str::to_owned)?;
    std::fs::write(path, blob).map_err(|e| e.to_string())
}
