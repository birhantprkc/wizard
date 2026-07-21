//! Build-time link fixes that cannot live in Cargo.toml.
//!
//! LuaJIT (pulled in by `mlua` with the `luajit` + `vendored` features) calls
//! `__clear_cache` on aarch64 to flush the instruction cache after emitting
//! JIT code. glibc and the dynamic libgcc path provide that symbol; a fully
//! static `aarch64-unknown-linux-musl` link with `musl-gcc` does not, so the
//! final link fails with an undefined reference. Pulling libgcc in closes the
//! gap. Other targets are unaffected.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target == "aarch64-unknown-linux-musl" {
        println!("cargo:rustc-link-arg=-lgcc");
    }
}
