//! Build-time link fixes that cannot live in Cargo.toml.
//!
//! LuaJIT (pulled in by `mlua` with the `luajit` + `vendored` features) calls
//! `__clear_cache` on aarch64 to flush the instruction cache after emitting
//! JIT code. glibc and the dynamic libgcc path provide that symbol; a fully
//! static `aarch64-unknown-linux-musl` link with `musl-gcc` does not, so the
//! final link fails with an undefined reference. Pulling libgcc in closes the
//! gap.
//!
//! The Android NDK has the same hole for a different reason: rustc links with
//! `-nodefaultlibs`, so clang never adds its compiler-rt builtins archive,
//! which is where `__clear_cache` lives there. The linker is asked where that
//! archive is rather than the path being spelled out, because it moves with
//! every NDK's clang version. Other targets are unaffected.

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTC_LINKER");
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "aarch64-unknown-linux-musl" {
        println!("cargo:rustc-link-arg=-lgcc");
    }
    if target.ends_with("-linux-android")
        && let Some(builtins) = android_builtins()
    {
        println!("cargo:rustc-link-arg={builtins}");
    }
}

/// The NDK clang's compiler-rt builtins archive, from the configured linker.
/// `None` when no linker is configured or it cannot answer, which leaves the
/// link to fail with the undefined symbol named.
fn android_builtins() -> Option<String> {
    let linker = std::env::var("RUSTC_LINKER").ok()?;
    let output = std::process::Command::new(linker)
        .arg("-print-libgcc-file-name")
        .output()
        .ok()?;
    let path = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (output.status.success() && std::path::Path::new(&path).is_file()).then_some(path)
}
