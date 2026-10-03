//! Turns the feature set and the target into the `crypto_*` cfgs the backends are compiled under:
//! a feature only takes effect on a target that can host the backend.

use std::env;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rustc-check-cfg=cfg(crypto_apple, crypto_cng, crypto_openssl, crypto_rustcrypto)");
    let feature = |name: &str| env::var_os(format!("CARGO_FEATURE_{name}")).is_some();
    let target = |name: &str| env::var(format!("CARGO_CFG_TARGET_{name}")).unwrap_or_default();
    let unix = env::var_os("CARGO_CFG_UNIX").is_some();

    let apple = feature("APPLE") && target("VENDOR") == "apple";
    let cng = feature("CNG") && target("OS") == "windows";
    let openssl = feature("OPENSSL") && unix && target("OS") != "android";
    let rustcrypto = feature("RUSTCRYPTO");
    for (name, on) in [("apple", apple), ("cng", cng), ("openssl", openssl), ("rustcrypto", rustcrypto)] {
        if on {
            println!("cargo::rustc-cfg=crypto_{name}");
        }
    }
    if !(apple || cng || openssl || rustcrypto) {
        println!(
            "cargo::error=lumen-crypto: no backend can be built for {}: enable `rustcrypto`, or the system backend of the target \
             (`apple` on Apple, `cng` on Windows, `openssl` on other unix)",
            env::var("TARGET").unwrap_or_default()
        );
    }
}
