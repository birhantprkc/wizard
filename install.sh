#!/usr/bin/env bash
#
# Wizard installer (llama.cpp-powered local models).
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
#
# Steps:
#   1. Detect OS and CPU architecture
#   2. Install llama.cpp's `llama-server` from official GitHub releases if absent
#   3. Select a model tier based on available VRAM (or system RAM on CPU-only)
#   4. Download the matching Qwen3 GGUF (Q4_K_M) from Hugging Face
#   5. Download the `wizard` binary from GitHub releases
#   6. Write ~/.wizard/config.toml (never clobbers an existing one)
#
# No server is started here: wizard launches llama-server itself on first run.
# Set WIZARD_USE_OLLAMA=1 for the previous Ollama-based flow.
#
# Environment variables:
#   WIZARD_INSTALL_DIR           where to place the binary    (default /usr/local/bin)
#   WIZARD_MODEL                 force a specific model tier  (default auto-detected)
#   WIZARD_SKIP_MODEL_PULL       1 = skip the model download  (default 0)
#   WIZARD_SKIP_LLAMACPP_INSTALL 1 = llama-server managed elsewhere (default 0)
#   WIZARD_USE_OLLAMA            1 = use Ollama instead of llama.cpp (default 0)
#   WIZARD_SKIP_OLLAMA_INSTALL   1 = Ollama managed elsewhere (default 0)
#   WIZARD_WITH_TOOLCHAIN        1 = eagerly install a Rust toolchain for deep evolve (default 0)
#   WIZARD_REPO                  owner/repo to install from   (default teddytennant/wizard)
#   WIZARD_REF                   git ref/branch when building from source (default main)
#   WIZARD_BUILD_FROM_SOURCE     1 = build from source instead of downloading a release (default 0)
#   WIZARD_BESPOKE               1 = start from scratch: skip writing config.toml and the
#                                    model download, so the first `wizard` run launches the
#                                    interactive onboarding wizard            (default 0)

set -euo pipefail

# --- defaults -----------------------------------------------------------

WIZARD_INSTALL_DIR="${WIZARD_INSTALL_DIR:-/usr/local/bin}"
WIZARD_MODEL="${WIZARD_MODEL:-}"
WIZARD_SKIP_MODEL_PULL="${WIZARD_SKIP_MODEL_PULL:-0}"
WIZARD_SKIP_LLAMACPP_INSTALL="${WIZARD_SKIP_LLAMACPP_INSTALL:-0}"
WIZARD_USE_OLLAMA="${WIZARD_USE_OLLAMA:-0}"
WIZARD_SKIP_OLLAMA_INSTALL="${WIZARD_SKIP_OLLAMA_INSTALL:-0}"
WIZARD_WITH_TOOLCHAIN="${WIZARD_WITH_TOOLCHAIN:-0}"
WIZARD_REPO="${WIZARD_REPO:-teddytennant/wizard}"
WIZARD_REF="${WIZARD_REF:-main}"
WIZARD_BUILD_FROM_SOURCE="${WIZARD_BUILD_FROM_SOURCE:-0}"
WIZARD_BESPOKE="${WIZARD_BESPOKE:-0}"

REPO="${WIZARD_REPO}"
RELEASE_BASE="https://github.com/${WIZARD_REPO}/releases/latest/download"
LLAMACPP_REPO="ggml-org/llama.cpp"
LLAMACPP_URL="http://127.0.0.1:8080"
LLAMA_BIN_DIR="$HOME/.wizard/bin"
MODELS_DIR="$HOME/.wizard/models"
OLLAMA_URL="http://127.0.0.1:11434"

ARCH=""
MODEL=""
GGUF_FILE=""
GGUF_URL=""
GGUF_PATH=""
MEM_GB=0
MEM_SOURCE=""
BINARY_INSTALLED=0
INSTALLED_PATH=""

TMP_DIR="$(mktemp -d)"
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT

# --- output helpers -----------------------------------------------------

say()  { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# --- platform detection -------------------------------------------------

detect_platform() {
    local os arch
    os="$(uname -s)"
    case "$os" in
        Linux) ;;
        Darwin)
            die "macOS is not supported in Wizard v0.1 — Linux only for now. macOS support is planned; sorry!"
            ;;
        *)
            die "unsupported operating system: $os (Wizard v0.1 supports Linux only)"
            ;;
    esac

    arch="$(uname -m)"
    case "$arch" in
        x86_64 | amd64)  ARCH="x86_64" ;;
        aarch64 | arm64) ARCH="aarch64" ;;
        *)
            die "unsupported CPU architecture: $arch (need x86_64 or aarch64)"
            ;;
    esac

    say "Platform: linux/${ARCH}"
}

require_curl() {
    command -v curl >/dev/null 2>&1 || die "curl is required but was not found on PATH"
}

# --- llama.cpp ----------------------------------------------------------

llamacpp_asset_url() {
    # $1 = release asset variant, e.g. "ubuntu-x64" or "ubuntu-vulkan-x64".
    # Picks the newest release that actually carries the asset — the most
    # recent tag can still be mid-upload and missing some platforms.
    local url tag
    url="$(curl -fsSL "https://api.github.com/repos/${LLAMACPP_REPO}/releases?per_page=8" 2>/dev/null \
        | grep -o "https://[^\"]*/llama-b[0-9]*-bin-${1}\.tar\.gz" | head -n1 || true)"
    if [ -n "$url" ]; then
        printf '%s' "$url"
        return
    fi
    # API unavailable (rate limit, proxy): derive the tag from the
    # /releases/latest redirect and verify the constructed URL exists.
    tag="$(curl -fsI -o /dev/null -w '%{redirect_url}' \
        "https://github.com/${LLAMACPP_REPO}/releases/latest" 2>/dev/null || true)"
    tag="${tag##*/}"
    if [ -z "$tag" ] || [ "$tag" = "latest" ]; then
        return 1
    fi
    url="https://github.com/${LLAMACPP_REPO}/releases/download/${tag}/llama-${tag}-bin-${1}.tar.gz"
    curl -fsI -o /dev/null "$url" 2>/dev/null || return 1
    printf '%s' "$url"
}

have_vulkan_loader() {
    command -v vulkaninfo >/dev/null 2>&1 && return 0
    ldconfig -p 2>/dev/null | grep -q 'libvulkan\.so'
}

llamacpp_variants() {
    # Candidate release-asset variants, preferred first. llama.cpp ships no
    # Linux CUDA release asset; Vulkan is the prebuilt GPU backend (and it
    # falls back to CPU at runtime when no usable GPU is present), so try it
    # when a GPU and a Vulkan loader were detected, with plain CPU as the
    # safe fallback.
    local suffix="x64"
    [ "$ARCH" = "aarch64" ] && suffix="arm64"
    case "$MEM_SOURCE" in
        "GPU VRAM"*)
            if have_vulkan_loader; then
                printf 'ubuntu-vulkan-%s\n' "$suffix"
            fi
            ;;
    esac
    printf 'ubuntu-%s\n' "$suffix"
}

expose_llama_server() {
    # wizard looks for llama-server on PATH when it starts the server
    # itself, so link it next to the wizard binary when possible.
    local src="${LLAMA_BIN_DIR}/llama-server"
    if [ ! -e "${WIZARD_INSTALL_DIR}/llama-server" ]; then
        if [ -d "$WIZARD_INSTALL_DIR" ] && [ -w "$WIZARD_INSTALL_DIR" ]; then
            ln -sfn "$src" "${WIZARD_INSTALL_DIR}/llama-server"
        elif command -v sudo >/dev/null 2>&1; then
            say "Need elevated permissions to link llama-server into ${WIZARD_INSTALL_DIR}"
            sudo ln -sfn "$src" "${WIZARD_INSTALL_DIR}/llama-server" || true
        fi
    fi
    if ! command -v llama-server >/dev/null 2>&1; then
        case ":$PATH:" in
            *":${LLAMA_BIN_DIR}:"*) ;;
            *) warn "${LLAMA_BIN_DIR} is not on your PATH — add it so wizard can find llama-server" ;;
        esac
    fi
}

install_llamacpp() {
    if [ "$WIZARD_SKIP_LLAMACPP_INSTALL" = "1" ]; then
        say "Skipping llama.cpp install (WIZARD_SKIP_LLAMACPP_INSTALL=1)"
        return
    fi
    if command -v llama-server >/dev/null 2>&1; then
        say "llama-server already installed ($(command -v llama-server))"
        return
    fi
    if [ -x "${LLAMA_BIN_DIR}/llama-server" ]; then
        say "llama-server already installed at ${LLAMA_BIN_DIR}/llama-server"
        expose_llama_server
        return
    fi

    # The Vulkan-vs-CPU choice needs to know whether a GPU is present.
    [ -n "$MEM_SOURCE" ] || detect_memory

    say "Installing llama-server (llama.cpp official releases) ..."
    local variant url archive dir bin dest
    for variant in $(llamacpp_variants); do
        url="$(llamacpp_asset_url "$variant" || true)"
        if [ -z "$url" ]; then
            warn "no llama.cpp release asset found for ${variant}"
            continue
        fi
        archive="${TMP_DIR}/llamacpp-${variant}.tar.gz"
        say "Downloading ${url##*/} ..."
        if ! curl -fL --progress-bar -o "$archive" "$url"; then
            warn "download failed for ${url##*/}"
            continue
        fi
        dir="${TMP_DIR}/llamacpp-${variant}"
        mkdir -p "$dir"
        if ! tar -xzf "$archive" -C "$dir"; then
            warn "could not extract ${url##*/}"
            continue
        fi
        bin="$(find "$dir" -type f -name llama-server | head -n1 || true)"
        if [ -z "$bin" ]; then
            warn "no llama-server binary inside ${url##*/}"
            continue
        fi
        chmod 755 "$bin"
        # Sanity check before keeping it — a Vulkan build without a usable
        # loader (or a glibc mismatch) fails here, and we try the next variant.
        if ! "$bin" --version >/dev/null 2>&1; then
            warn "the ${variant} build does not run on this system — trying the next variant"
            continue
        fi
        # Keep the whole release tree: llama-server resolves its shared
        # libraries via an \$ORIGIN runpath, so the .so files must stay next
        # to the real binary. PATH only needs the symlink.
        dest="$HOME/.wizard/llama.cpp"
        rm -rf "$dest"
        mkdir -p "$dest"
        cp -R "$(dirname "$bin")"/. "$dest/"
        mkdir -p "$LLAMA_BIN_DIR"
        ln -sfn "${dest}/llama-server" "${LLAMA_BIN_DIR}/llama-server"
        say "Installed llama-server to ${dest} (${variant} build)"
        expose_llama_server
        return
    done

    warn "could not install a prebuilt llama-server for linux/${ARCH}"
    warn "install it yourself — wizard will start it automatically once it is on PATH:"
    printf '\n' >&2
    printf '    brew install llama.cpp                  # Homebrew / Linuxbrew\n' >&2
    printf '    nix profile install nixpkgs#llama-cpp   # Nix / NixOS\n' >&2
    printf '    https://github.com/%s — build from source\n' "$LLAMACPP_REPO" >&2
    printf '\n' >&2
}

# --- ollama (WIZARD_USE_OLLAMA=1) ---------------------------------------

ollama_running() {
    curl -fsS --max-time 3 "${OLLAMA_URL}/api/tags" >/dev/null 2>&1
}

install_ollama() {
    if [ "$WIZARD_SKIP_OLLAMA_INSTALL" = "1" ]; then
        say "Skipping Ollama install (WIZARD_SKIP_OLLAMA_INSTALL=1)"
        return
    fi
    if command -v ollama >/dev/null 2>&1; then
        say "Ollama already installed"
        return
    fi
    say "Installing Ollama (official install script) ..."
    curl -fsSL https://ollama.com/install.sh | sh \
        || die "Ollama installation failed — install it manually from https://ollama.com/download and re-run"
}

start_ollama() {
    if ollama_running; then
        say "Ollama server is running at ${OLLAMA_URL}"
        return
    fi

    if ! command -v ollama >/dev/null 2>&1; then
        if [ "$WIZARD_SKIP_OLLAMA_INSTALL" = "1" ]; then
            warn "Ollama is neither installed nor reachable at ${OLLAMA_URL}; continuing anyway (WIZARD_SKIP_OLLAMA_INSTALL=1)"
            return
        fi
        die "ollama binary not found after install — check the Ollama installation"
    fi

    say "Starting Ollama server ..."
    if command -v systemctl >/dev/null 2>&1 \
        && systemctl list-unit-files ollama.service >/dev/null 2>&1; then
        if [ "$(id -u)" -eq 0 ]; then
            systemctl start ollama || true
        elif command -v sudo >/dev/null 2>&1; then
            sudo systemctl start ollama || true
        fi
    fi

    if ! ollama_running; then
        mkdir -p "$HOME/.wizard/logs"
        nohup ollama serve >"$HOME/.wizard/logs/ollama.log" 2>&1 &
    fi

    local _try
    for _try in $(seq 1 30); do
        if ollama_running; then
            say "Ollama server is up"
            return
        fi
        sleep 1
    done
    die "Ollama server did not come up at ${OLLAMA_URL} within 30s — try 'ollama serve' manually, then re-run"
}

# --- model tier selection -----------------------------------------------

is_uint() {
    # True if $1 is a non-empty string of digits (safe for arithmetic).
    case "$1" in
        '' | *[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

detect_memory() {
    # Prefer GPU VRAM (largest single GPU — the model must fit on one card),
    # fall back to system RAM as a heuristic on CPU-only machines.
    # On total detection failure, leave MEM_SOURCE empty so the caller can
    # fall back to the smallest tier instead of dying.

    # NVIDIA: nvidia-smi can exist but print nothing or garbage (driver
    # mismatch, headless cloud images) — only trust a plain number.
    if command -v nvidia-smi >/dev/null 2>&1; then
        local vram_mib
        vram_mib="$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null \
            | sort -nr | head -n1 | tr -d '[:space:]' || true)"
        if is_uint "$vram_mib" && [ "$vram_mib" -gt 0 ]; then
            MEM_GB=$((vram_mib / 1024))
            MEM_SOURCE="GPU VRAM (nvidia-smi)"
            return
        fi
        warn "nvidia-smi is present but did not report usable VRAM (driver mismatch?) — ignoring it"
    fi

    # AMD: rocm-smi if present, else the amdgpu sysfs VRAM counter (bytes).
    if command -v rocm-smi >/dev/null 2>&1; then
        local vram_b
        vram_b="$(rocm-smi --showmeminfo vram --csv 2>/dev/null \
            | awk -F, '$2 ~ /^[0-9]+$/ {print $2}' | sort -nr | head -n1 || true)"
        if is_uint "$vram_b" && [ "$vram_b" -gt 0 ]; then
            MEM_GB=$((vram_b / 1024 / 1024 / 1024))
            MEM_SOURCE="GPU VRAM (rocm-smi)"
            return
        fi
        warn "rocm-smi is present but did not report usable VRAM — ignoring it"
    fi
    local sysfs_file vram_b best=0
    for sysfs_file in /sys/class/drm/card[0-9]*/device/mem_info_vram_total; do
        [ -r "$sysfs_file" ] || continue
        vram_b="$(cat "$sysfs_file" 2>/dev/null || true)"
        if is_uint "$vram_b" && [ "$vram_b" -gt "$best" ]; then
            best="$vram_b"
        fi
    done
    if [ "$best" -gt 0 ]; then
        MEM_GB=$((best / 1024 / 1024 / 1024))
        MEM_SOURCE="GPU VRAM (sysfs amdgpu)"
        return
    fi

    local mem_kb
    mem_kb="$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null || true)"
    if is_uint "$mem_kb" && [ "$mem_kb" -gt 0 ]; then
        MEM_GB=$((mem_kb / 1024 / 1024))
        MEM_SOURCE="system RAM (no GPU detected)"
        return
    fi

    MEM_GB=0
    MEM_SOURCE=""
}

select_model() {
    if [ -n "$WIZARD_MODEL" ]; then
        MODEL="$WIZARD_MODEL"
        say "Model forced via WIZARD_MODEL: ${MODEL}"
        return
    fi

    detect_memory
    if [ -z "$MEM_SOURCE" ]; then
        MODEL="qwen3.5:9b"
        warn "could not detect GPU VRAM or system RAM — falling back to the smallest model tier"
        say "Selected model tier: ${MODEL} (override with WIZARD_MODEL=<tag>)"
        return
    fi
    say "Detected ${MEM_GB} GB of ${MEM_SOURCE}"

    if [ "$MEM_GB" -ge 24 ]; then
        MODEL="qwen3.6:35b"
    elif [ "$MEM_GB" -ge 18 ]; then
        MODEL="qwen3.6:27b"
    elif [ "$MEM_GB" -ge 8 ]; then
        MODEL="qwen3.5:9b"
    else
        MODEL="qwen3.5:9b"
        warn "less than 8 GB available — ${MODEL} will run on CPU / partial offload and may be slow"
    fi
    say "Selected model tier: ${MODEL}"
}

gguf_for_model() {
    # Map a model tier tag to its Q4_K_M GGUF on Hugging Face. Leaves
    # GGUF_FILE/GGUF_URL empty for tags with no known download.
    GGUF_FILE=""
    GGUF_URL=""
    case "$1" in
        qwen3.6:35b)
            GGUF_FILE="Qwen3.6-35B-A3B-UD-Q4_K_M.gguf"
            GGUF_URL="https://huggingface.co/unsloth/Qwen3.6-35B-A3B-GGUF/resolve/main/${GGUF_FILE}"
            ;;
        qwen3.6:27b)
            GGUF_FILE="Qwen3.6-27B-Q4_K_M.gguf"
            GGUF_URL="https://huggingface.co/unsloth/Qwen3.6-27B-GGUF/resolve/main/${GGUF_FILE}"
            ;;
        qwen3.5:9b)
            GGUF_FILE="Qwen3.5-9B-Q4_K_M.gguf"
            GGUF_URL="https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/resolve/main/${GGUF_FILE}"
            ;;
    esac
}

download_gguf() {
    if [ "$WIZARD_BESPOKE" = "1" ]; then
        say "Bespoke install: skipping model download — onboarding will pick your model on first run"
        return
    fi
    gguf_for_model "$MODEL"
    if [ -z "$GGUF_FILE" ]; then
        warn "no known GGUF download for '${MODEL}' — set gguf_path in ~/.wizard/config.toml to your own .gguf file"
        return
    fi
    GGUF_PATH="${MODELS_DIR}/${GGUF_FILE}"
    if [ -f "$GGUF_PATH" ]; then
        say "Model already downloaded: ${GGUF_PATH}"
        return
    fi
    if [ "$WIZARD_SKIP_MODEL_PULL" = "1" ]; then
        say "Skipping model download (WIZARD_SKIP_MODEL_PULL=1)"
        return
    fi
    mkdir -p "$MODELS_DIR"
    say "Downloading ${GGUF_FILE} from Hugging Face (several GB — this can take a while) ..."
    # -C - resumes a partial download from an interrupted earlier run.
    if ! curl -fL -C - --progress-bar -o "${GGUF_PATH}.partial" "$GGUF_URL"; then
        die "failed to download ${GGUF_URL} — check connectivity and disk space, then re-run (the download resumes)"
    fi
    mv "${GGUF_PATH}.partial" "$GGUF_PATH"
    say "Saved ${GGUF_PATH}"
}

pull_model() {
    if [ "$WIZARD_BESPOKE" = "1" ]; then
        say "Bespoke install: skipping model pull — onboarding will pick your model on first run"
        return
    fi
    if [ "$WIZARD_SKIP_MODEL_PULL" = "1" ]; then
        say "Skipping model pull (WIZARD_SKIP_MODEL_PULL=1)"
        return
    fi
    if ! command -v ollama >/dev/null 2>&1; then
        warn "ollama binary not found; skipping model pull — run 'ollama pull ${MODEL}' yourself"
        return
    fi
    say "Pulling ${MODEL} from the Ollama library (this can take a while) ..."
    ollama pull "$MODEL" \
        || die "failed to pull ${MODEL} — check connectivity, then run 'ollama pull ${MODEL}' manually"
}

# --- wizard binary ------------------------------------------------------

place_binary() {
    # $1 = path to the extracted binary
    local src="$1"
    chmod 755 "$src"

    if [ -d "$WIZARD_INSTALL_DIR" ] && [ -w "$WIZARD_INSTALL_DIR" ]; then
        install -m 755 "$src" "${WIZARD_INSTALL_DIR}/wizard"
    elif [ ! -e "$WIZARD_INSTALL_DIR" ] && mkdir -p "$WIZARD_INSTALL_DIR" 2>/dev/null; then
        install -m 755 "$src" "${WIZARD_INSTALL_DIR}/wizard"
    elif command -v sudo >/dev/null 2>&1; then
        say "Need elevated permissions to write to ${WIZARD_INSTALL_DIR}"
        sudo mkdir -p "$WIZARD_INSTALL_DIR"
        sudo install -m 755 "$src" "${WIZARD_INSTALL_DIR}/wizard"
    else
        local fallback="$HOME/.local/bin"
        warn "${WIZARD_INSTALL_DIR} is not writable and sudo is unavailable — installing to ${fallback} instead"
        mkdir -p "$fallback"
        install -m 755 "$src" "${fallback}/wizard"
        case ":$PATH:" in
            *":${fallback}:"*) ;;
            *) warn "${fallback} is not on your PATH — add it to your shell profile" ;;
        esac
        INSTALLED_PATH="${fallback}/wizard"
        return
    fi
    INSTALLED_PATH="${WIZARD_INSTALL_DIR}/wizard"
}

verify_checksum() {
    # $1 = path to the downloaded tarball, $2 = asset name.
    # Missing checksums.txt (older release) is a warning; a mismatch is fatal.
    local tarball="$1" asset="$2" sums="${TMP_DIR}/checksums.txt" expected actual
    if [ ! -f "$sums" ] \
        && ! curl -fsSL -o "$sums" "${RELEASE_BASE}/checksums.txt" 2>/dev/null; then
        warn "release has no checksums.txt — skipping checksum verification"
        return
    fi
    expected="$(awk -v a="$asset" '$2 == a {print $1; exit}' "$sums" || true)"
    if [ -z "$expected" ]; then
        warn "checksums.txt has no entry for ${asset} — skipping checksum verification"
        return
    fi
    if ! command -v sha256sum >/dev/null 2>&1; then
        warn "sha256sum not found on PATH — skipping checksum verification"
        return
    fi
    actual="$(sha256sum "$tarball" | awk '{print $1}')"
    if [ "$actual" != "$expected" ]; then
        die "checksum mismatch for ${asset} (expected ${expected}, got ${actual}) — the download may be corrupted or tampered with; aborting"
    fi
    say "Checksum verified for ${asset}"
}

download_binary() {
    say "Downloading wizard binary from GitHub releases (${REPO}) ..."
    local asset bin
    for asset in "wizard-${ARCH}-unknown-linux-gnu.tar.gz" "wizard-linux-${ARCH}.tar.gz"; do
        if curl -fsSL -o "${TMP_DIR}/${asset}" "${RELEASE_BASE}/${asset}" 2>/dev/null; then
            verify_checksum "${TMP_DIR}/${asset}" "${asset}"
            tar -xzf "${TMP_DIR}/${asset}" -C "$TMP_DIR" || continue
            bin="$(find "$TMP_DIR" -type f -name wizard | head -n1 || true)"
            if [ -n "$bin" ]; then
                place_binary "$bin"
                BINARY_INSTALLED=1
                say "Installed wizard to ${INSTALLED_PATH}"
                return
            fi
        fi
    done

    warn "could not download a prebuilt wizard binary for linux/${ARCH}"
    warn "you can build it from source instead (requires a Rust toolchain):"
    printf '\n' >&2
    printf '    git clone https://github.com/%s ~/.wizard/src\n' "$REPO" >&2
    printf '    cd ~/.wizard/src && cargo build --release\n' >&2
    printf '    install -m 755 target/release/wizard %s/wizard\n' "$WIZARD_INSTALL_DIR" >&2
    printf '\n' >&2
}

# --- rust toolchain (optional, for deep evolve) -------------------------

ensure_rust_toolchain() {
    # Add ~/.cargo/bin to PATH for this session in case rustup was previously
    # installed without modifying the shell profile (--no-modify-path).
    case ":${PATH}:" in
        *":$HOME/.cargo/bin:"*) ;;
        *) export PATH="$HOME/.cargo/bin:$PATH" ;;
    esac
    if command -v cargo >/dev/null 2>&1; then
        say "Rust toolchain already present (cargo found)"
        return
    fi
    say "Installing minimal Rust toolchain via rustup ..."
    curl -fsSL https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path \
        || die "rustup installation failed — install Rust manually from https://rustup.rs"
    export PATH="$HOME/.cargo/bin:$PATH"
    command -v cargo >/dev/null 2>&1 \
        || die "cargo not found after rustup install — check ~/.cargo/bin"
    say "Rust toolchain installed under ~/.cargo"
}

install_toolchain() {
    if [ "$WIZARD_WITH_TOOLCHAIN" != "1" ]; then
        return
    fi
    say "Ensuring Rust toolchain for deep evolve (WIZARD_WITH_TOOLCHAIN=1) ..."
    ensure_rust_toolchain
}

# --- build from source --------------------------------------------------

build_from_source() {
    command -v git >/dev/null 2>&1 \
        || die "git is required to build from source but was not found on PATH"
    say "Building wizard from source (${WIZARD_REPO}@${WIZARD_REF}) ..."
    local src_dir="${TMP_DIR}/wizard-src"
    git clone --depth 1 --branch "$WIZARD_REF" \
        "https://github.com/${WIZARD_REPO}" "$src_dir" \
        || die "git clone failed — check WIZARD_REPO (${WIZARD_REPO}) and WIZARD_REF (${WIZARD_REF})"
    ensure_rust_toolchain
    say "Running cargo build --release (this may take several minutes) ..."
    ( cd "$src_dir" && cargo build --release ) \
        || die "cargo build --release failed — see output above for details"
    local bin="${src_dir}/target/release/wizard"
    [ -f "$bin" ] \
        || die "build succeeded but target/release/wizard not found in ${src_dir}"
    place_binary "$bin"
    BINARY_INSTALLED=1
    say "Installed wizard (built from source) to ${INSTALLED_PATH}"
}

# --- config -------------------------------------------------------------

write_config() {
    local cfg="$HOME/.wizard/config.toml"
    mkdir -p "$HOME/.wizard"
    if [ "$WIZARD_BESPOKE" = "1" ]; then
        if [ -f "$cfg" ]; then
            say "Bespoke install requested, but a config already exists at ${cfg} — leaving it untouched"
            say "Run 'wizard --onboard' to reconfigure from scratch"
        else
            say "Bespoke install: no config written — the first 'wizard' run will start onboarding"
        fi
        return
    fi
    if [ -f "$cfg" ]; then
        say "Existing config found at ${cfg} — leaving it untouched"
        say "To switch models or providers, edit it or use /provider inside wizard"
        return
    fi
    say "Writing ${cfg}"
    if [ "$WIZARD_USE_OLLAMA" = "1" ]; then
        cat >"$cfg" <<EOF
# Wizard configuration — see https://github.com/${REPO}
active_provider = "local"
mode = "genie"
auto_approve = false
max_steps = 25

[[providers]]
name = "local"
kind = "ollama"
base_url = "${OLLAMA_URL}"
model = "${MODEL}"
EOF
        return
    fi
    # llama-server ignores the request model name; the GGUF stem keeps
    # wizard's labels meaningful. gguf_path lets wizard start the server.
    local model_name="$MODEL"
    if [ -n "$GGUF_FILE" ]; then
        model_name="${GGUF_FILE%.gguf}"
    fi
    cat >"$cfg" <<EOF
# Wizard configuration — see https://github.com/${REPO}
active_provider = "local"
mode = "genie"
auto_approve = false
max_steps = 25

[[providers]]
name = "local"
kind = "llamacpp"
base_url = "${LLAMACPP_URL}"
model = "${model_name}"
EOF
    if [ -n "$GGUF_PATH" ]; then
        printf 'gguf_path = "%s"\n' "$GGUF_PATH" >>"$cfg"
    fi
}

# --- main ---------------------------------------------------------------

main() {
    say "Wizard installer"
    require_curl
    detect_platform

    if [ "$WIZARD_USE_OLLAMA" = "1" ]; then
        say "Using Ollama as the local provider (WIZARD_USE_OLLAMA=1)"
        install_ollama
        start_ollama
        select_model
        pull_model
    else
        install_llamacpp
        select_model
        download_gguf
    fi

    if [ "$WIZARD_BUILD_FROM_SOURCE" = "1" ]; then
        build_from_source
    else
        download_binary
        if [ "$BINARY_INSTALLED" != "1" ]; then
            say "No prebuilt binary found; falling back to building from source ..."
            build_from_source
        fi
    fi

    install_toolchain
    write_config

    printf '\n'
    if [ "$BINARY_INSTALLED" = "1" ]; then
        if [ "$WIZARD_BESPOKE" = "1" ]; then
            say "Done. Run 'wizard' to start onboarding (pick your model, provider, and gateway)."
        elif [ "$WIZARD_USE_OLLAMA" = "1" ]; then
            say "Done. Run: wizard"
        else
            say "Done. Run: wizard — it starts llama-server with your model automatically."
        fi
    else
        say "Setup finished, but the wizard binary was NOT installed — see the build-from-source steps above."
    fi
}

main "$@"
