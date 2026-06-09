#!/usr/bin/env bash
#
# Wizard installer (official models).
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
#
# Steps:
#   1. Detect OS and CPU architecture
#   2. Install Ollama if absent
#   3. Start the Ollama server if it is down
#   4. Select a model tier based on available VRAM (or system RAM on CPU-only)
#   5. Pull the model from the official Ollama library
#   6. Download the `wizard` binary from GitHub releases
#   7. Write ~/.wizard/config.toml (never clobbers an existing one)
#
# Environment variables:
#   WIZARD_INSTALL_DIR         where to place the binary    (default /usr/local/bin)
#   WIZARD_MODEL               force a specific model tag   (default auto-detected)
#   WIZARD_SKIP_MODEL_PULL     1 = skip `ollama pull`       (default 0)
#   WIZARD_SKIP_OLLAMA_INSTALL 1 = Ollama managed elsewhere (default 0)
#   WIZARD_WITH_TOOLCHAIN      1 = eagerly install a Rust toolchain for deep evolve (default 0)

set -euo pipefail

# --- defaults -----------------------------------------------------------

WIZARD_INSTALL_DIR="${WIZARD_INSTALL_DIR:-/usr/local/bin}"
WIZARD_MODEL="${WIZARD_MODEL:-}"
WIZARD_SKIP_MODEL_PULL="${WIZARD_SKIP_MODEL_PULL:-0}"
WIZARD_SKIP_OLLAMA_INSTALL="${WIZARD_SKIP_OLLAMA_INSTALL:-0}"
WIZARD_WITH_TOOLCHAIN="${WIZARD_WITH_TOOLCHAIN:-0}"

REPO="teddytennant/wizard"
RELEASE_BASE="https://github.com/${REPO}/releases/latest/download"
OLLAMA_URL="http://127.0.0.1:11434"

ARCH=""
MODEL=""
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

# --- ollama -------------------------------------------------------------

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

    local i
    for i in $(seq 1 30); do
        if ollama_running; then
            say "Ollama server is up"
            return
        fi
        sleep 1
    done
    die "Ollama server did not come up at ${OLLAMA_URL} within 30s — try 'ollama serve' manually, then re-run"
}

# --- model tier selection -----------------------------------------------

detect_memory() {
    # Prefer GPU VRAM (largest single GPU — the model must fit on one card),
    # fall back to system RAM as a heuristic on CPU-only machines.
    if command -v nvidia-smi >/dev/null 2>&1; then
        local vram_mib
        vram_mib="$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null \
            | sort -nr | head -n1 | tr -d '[:space:]' || true)"
        if [ -n "$vram_mib" ] && [ "$vram_mib" -gt 0 ] 2>/dev/null; then
            MEM_GB=$((vram_mib / 1024))
            MEM_SOURCE="GPU VRAM (nvidia-smi)"
            return
        fi
    fi
    local mem_kb
    mem_kb="$(awk '/^MemTotal:/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)"
    MEM_GB=$((mem_kb / 1024 / 1024))
    MEM_SOURCE="system RAM (no GPU detected)"
}

select_model() {
    if [ -n "$WIZARD_MODEL" ]; then
        MODEL="$WIZARD_MODEL"
        say "Model forced via WIZARD_MODEL: ${MODEL}"
        return
    fi

    detect_memory
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

pull_model() {
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

download_binary() {
    say "Downloading wizard binary from GitHub releases (${REPO}) ..."
    local asset bin
    for asset in "wizard-${ARCH}-unknown-linux-gnu.tar.gz" "wizard-linux-${ARCH}.tar.gz"; do
        if curl -fsSL -o "${TMP_DIR}/${asset}" "${RELEASE_BASE}/${asset}" 2>/dev/null; then
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

install_toolchain() {
    if [ "$WIZARD_WITH_TOOLCHAIN" != "1" ]; then
        return
    fi
    if command -v cargo >/dev/null 2>&1; then
        say "Rust toolchain already present (cargo found); skipping toolchain install"
        return
    fi
    say "Installing minimal Rust toolchain for deep evolve (WIZARD_WITH_TOOLCHAIN=1) ..."
    curl -fsSL https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain stable --no-modify-path \
        || die "rustup installation failed — install Rust manually from https://rustup.rs"
    say "Toolchain installed under ~/.cargo (deep evolve will find it)"
}

# --- config -------------------------------------------------------------

write_config() {
    local cfg="$HOME/.wizard/config.toml"
    mkdir -p "$HOME/.wizard"
    if [ -f "$cfg" ]; then
        say "Existing config found at ${cfg} — leaving it untouched"
        say "To switch models, edit it: model = \"${MODEL}\""
        return
    fi
    say "Writing ${cfg}"
    cat >"$cfg" <<EOF
# Wizard configuration — see https://github.com/${REPO}
model = "${MODEL}"
ollama_host = "${OLLAMA_URL}"
mode = "genie"
auto_approve = false
max_steps = 25
EOF
}

# --- main ---------------------------------------------------------------

main() {
    say "Wizard installer"
    require_curl
    detect_platform
    install_ollama
    start_ollama
    select_model
    pull_model
    download_binary
    install_toolchain
    write_config

    printf '\n'
    if [ "$BINARY_INSTALLED" = "1" ]; then
        say "Done. Run: wizard"
    else
        say "Setup finished, but the wizard binary was NOT installed — see the build-from-source steps above."
    fi
}

main "$@"
