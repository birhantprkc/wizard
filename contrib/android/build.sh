#!/usr/bin/env bash
# Build wizard for Android with the NDK, as the libwizard.so an APK ships in
# jniLibs/<abi>/ and runs from its nativeLibraryDir (`wizard acp` over stdio).
#
#   contrib/android/build.sh            # arm64-v8a and x86_64
#   contrib/android/build.sh x86_64     # one ABI
#
# Output: target/android/<abi>/libwizard.so, or $OUT_DIR/<abi>/libwizard.so.
#
# The NDK comes from $ANDROID_NDK_HOME when set. Otherwise it is built with
# Nix from the nixpkgs android/flake.lock pins, with a GC root at
# $WIZARD_ANDROID_CACHE/ndk (default ~/.cache/wizard-android) so it can be
# deleted along with that directory.
#
# What the app has to give the process, since an app has no /bin, no /tmp and
# no writable home of its own:
#   HOME=<filesDir>/home  WIZARD_HOME=<filesDir>/wizard  TMPDIR=<cacheDir>
#   SHELL=<nativeLibraryDir>/libbash.so   (else /system/bin/sh; or
#                                          [shell] program in config.toml)
#   BROWSER=<helper that hands a URL to the app>   (OAuth sign-in)
#   PATH  with the directory of any bundled tools (busybox applets) first
set -euo pipefail

repo=$(cd "$(dirname "$0")/../.." && pwd)
api=${ANDROID_API:-29}
cache=${WIZARD_ANDROID_CACHE:-$HOME/.cache/wizard-android}
out_dir=${OUT_DIR:-$repo/target/android}
abis=("$@")
[ ${#abis[@]} -gt 0 ] || abis=(arm64-v8a x86_64)

ndk=${ANDROID_NDK_HOME:-}
if [ -z "$ndk" ]; then
    mkdir -p "$cache"
    NIXPKGS_ACCEPT_ANDROID_SDK_LICENSE=1 nix build --impure --out-link "$cache/ndk" --expr "
      let
        nixpkgs = (builtins.getFlake \"path:$repo/android\").inputs.nixpkgs;
        pkgs = import nixpkgs {
          system = \"x86_64-linux\";
          config = { allowUnfree = true; android_sdk.accept_license = true; };
        };
      in pkgs.androidenv.androidPkgs.ndk-bundle"
    ndk=$(readlink -f "$cache/ndk")/libexec/android-sdk/ndk-bundle
fi
prebuilt=$ndk/toolchains/llvm/prebuilt/linux-x86_64
bin=$prebuilt/bin
[ -x "$bin/clang" ] || { echo "no NDK clang under $ndk" >&2; exit 1; }

# rquickjs-sys generates its Android bindings with bindgen, which needs
# libclang and the NDK's own headers rather than the host's.
export LIBCLANG_PATH=$prebuilt/lib
export CLANG_PATH=$bin/clang

for abi in "${abis[@]}"; do
    case $abi in
        arm64-v8a) triple=aarch64-linux-android ;;
        x86_64) triple=x86_64-linux-android ;;
        *) echo "unknown ABI $abi (arm64-v8a or x86_64)" >&2; exit 1 ;;
    esac
    var=${triple//-/_}
    upper=${var^^}
    export "CC_$var=$bin/$triple$api-clang"
    export "AR_$var=$bin/llvm-ar"
    export "CARGO_TARGET_${upper}_LINKER=$bin/$triple$api-clang"
    export "BINDGEN_EXTRA_CLANG_ARGS_$var=--sysroot=$prebuilt/sysroot"
    if command -v rustup >/dev/null; then
        rustup target add "$triple" >/dev/null
    fi

    (cd "$repo" && cargo build --locked --release --target "$triple")

    target_dir=${CARGO_TARGET_DIR:-$repo/target}
    mkdir -p "$out_dir/$abi"
    "$bin/llvm-strip" --strip-all -o "$out_dir/$abi/libwizard.so" \
        "$target_dir/$triple/release/wizard"
    size=$(du -h "$out_dir/$abi/libwizard.so" | cut -f1)
    echo "$out_dir/$abi/libwizard.so ($size)"
done
