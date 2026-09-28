//! Embeds the minisign public key the updater trusts.
//!
//! Normally that is `wizard-release.pub` at the repository root, the same key
//! `wizard update` compiles in. `WIZARD_GUI_TEST_RELEASE_KEY=<path to a .pub>`
//! swaps in a throwaway key for end-to-end runs that sign a fake release; such
//! a build also sets `cfg(wizard_test_release_key)`, which logs a warning at
//! every check and lets `WIZARD_GUI_TEST_CA` add a root certificate for a
//! local HTTPS feed. Release builds never set it.

use std::path::PathBuf;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(wizard_test_release_key)");
    println!("cargo:rerun-if-env-changed=WIZARD_GUI_TEST_RELEASE_KEY");
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let release_key = manifest.join("../../../wizard-release.pub");
    let source = match std::env::var_os("WIZARD_GUI_TEST_RELEASE_KEY").filter(|v| !v.is_empty()) {
        Some(path) => {
            println!("cargo:rustc-cfg=wizard_test_release_key");
            PathBuf::from(path)
        }
        None => release_key,
    };
    println!("cargo:rerun-if-changed={}", source.display());
    let key = std::fs::read_to_string(&source)
        .unwrap_or_else(|err| panic!("reading the release key {}: {err}", source.display()));
    let out = PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("release.pub");
    std::fs::write(out, key).expect("writing the embedded release key");
}
