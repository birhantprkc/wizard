#!/usr/bin/env bash
#
# Wizard BYOM (bring-your-own-model) installer.
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install-byom.sh | bash
#
# Same binary install as install.sh, but no automatic model pull: you choose
# the model interactively (any Ollama-compatible model — library tags, custom
# registry tags, local Modelfiles, or one already installed).
#
# Environment variables:
#   WIZARD_INSTALL_DIR         where to place the binary    (default /usr/local/bin)
#   WIZARD_MODEL               skip the interactive flow and use this tag as-is
#   WIZARD_SKIP_OLLAMA_INSTALL 1 = Ollama managed elsewhere (default 0)
#   WIZARD_WITH_TOOLCHAIN      1 = eagerly install a Rust toolchain for deep evolve (default 0)
#   WIZARD_REPO                owner/repo to install from   (default teddytennant/wizard)
#   WIZARD_REF                 git ref/tag when building from source
#                              (default: latest release tag, falling back to
#                              main only when the repo has no release)
#   WIZARD_BUILD_FROM_SOURCE   1 = build from source instead of downloading a release (default 0)
#
# Disclaimer: you choose the model. Wizard does not ship, endorse, or maintain
# third-party model weights; you are responsible for their licenses and terms.

set -euo pipefail

# --- defaults -----------------------------------------------------------

WIZARD_INSTALL_DIR="${WIZARD_INSTALL_DIR:-/usr/local/bin}"
WIZARD_MODEL="${WIZARD_MODEL:-}"
WIZARD_SKIP_OLLAMA_INSTALL="${WIZARD_SKIP_OLLAMA_INSTALL:-0}"
WIZARD_WITH_TOOLCHAIN="${WIZARD_WITH_TOOLCHAIN:-0}"
WIZARD_REPO="${WIZARD_REPO:-teddytennant/wizard}"
WIZARD_REF="${WIZARD_REF:-}"
WIZARD_BUILD_FROM_SOURCE="${WIZARD_BUILD_FROM_SOURCE:-0}"

REPO="${WIZARD_REPO}"
RELEASE_BASE="https://github.com/${WIZARD_REPO}/releases/latest/download"
OLLAMA_URL="http://127.0.0.1:11434"

ARCH=""
MODEL=""
BINARY_INSTALLED=0
INSTALLED_PATH=""

TMP_DIR="$(mktemp -d)"
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT

# --- output helpers -----------------------------------------------------

say()  { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

# Interactive prompt that works under `curl | bash` (stdin is the script,
# so read from the controlling terminal instead).
ask() {
    # $1 = variable name, $2 = prompt text
    local _var="$1" _msg="$2" _reply
    printf '%s' "$_msg" >/dev/tty
    IFS= read -r _reply </dev/tty
    eval "$_var=\$_reply"
}

require_tty() {
    if [ ! -e /dev/tty ] || ! { true </dev/tty; } 2>/dev/null; then
        die "BYOM setup is interactive and needs a terminal. Either run it from an interactive shell, or set WIZARD_MODEL=<tag> to skip the prompts."
    fi
}

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

download_release_asset() {
    # $1 = release asset name, $2 = output path.
    # Plain curl covers public releases; on a private repo the unauthenticated
    # asset URL returns plain 404, so fall back to an authenticated
    # `gh release download` when the gh CLI is available.
    local asset="$1" out="$2"
    if curl -fsSL -o "$out" "${RELEASE_BASE}/${asset}" 2>/dev/null; then
        return 0
    fi
    rm -f "$out"
    if command -v gh >/dev/null 2>&1; then
        if gh release download --repo "$REPO" --pattern "$asset" \
            --output "$out" 2>/dev/null; then
            return 0
        fi
        rm -f "$out"
    fi
    return 1
}

verify_checksum() {
    # $1 = path to the downloaded tarball, $2 = asset name.
    # Missing checksums.txt (older release) is a warning; a mismatch is fatal.
    local tarball="$1" asset="$2" sums="${TMP_DIR}/checksums.txt" expected actual
    if [ ! -f "$sums" ] \
        && ! download_release_asset "checksums.txt" "$sums"; then
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
        if download_release_asset "$asset" "${TMP_DIR}/${asset}"; then
            verify_checksum "${TMP_DIR}/${asset}" "${asset}"
            tar -xzf "${TMP_DIR}/${asset}" -C "$TMP_DIR" || continue
            bin="$(find "$TMP_DIR" -type f -name wizard | head -n1 || true)"
            if [ -z "$bin" ]; then
                warn "no wizard binary inside ${asset}"
                continue
            fi
            chmod 755 "$bin"
            # Sanity check before installing — catches a corrupt download or
            # a glibc mismatch instead of declaring success with a dud binary.
            if ! "$bin" --version >/dev/null 2>&1; then
                warn "the binary from ${asset} does not run on this system"
                continue
            fi
            place_binary "$bin"
            BINARY_INSTALLED=1
            say "Installed wizard to ${INSTALLED_PATH}"
            return
        fi
    done

    warn "could not download a prebuilt wizard binary for linux/${ARCH}"
    warn "(a 404 here also happens when ${REPO} is private — 'gh auth login' enables an authenticated download)"
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

resolve_source_ref() {
    # Prefer the newest published release tag so a source build compiles a
    # known-good, CI-passed commit instead of the moving tip of main. An
    # explicit WIZARD_REF always wins; main is the last resort, used only
    # when the repo has no published release at all.
    local tag
    if [ -n "$WIZARD_REF" ]; then
        printf '%s' "$WIZARD_REF"
        return
    fi
    tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
        | grep -o '"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"' | head -n1 \
        | sed -E 's/.*"([^"]+)"$/\1/' || true)"
    if [ -z "$tag" ]; then
        # API unavailable (rate limit, proxy): derive the tag from the
        # /releases/latest redirect instead.
        tag="$(curl -fsI -o /dev/null -w '%{redirect_url}' \
            "https://github.com/${REPO}/releases/latest" 2>/dev/null || true)"
        tag="${tag##*/}"
        [ "$tag" = "latest" ] && tag=""
    fi
    if [ -n "$tag" ]; then
        printf '%s' "$tag"
    else
        warn "no published release found for ${REPO} — building from main (unreviewed tip; set WIZARD_REF to pin a ref)"
        printf 'main'
    fi
}

build_from_source() {
    command -v git >/dev/null 2>&1 \
        || die "git is required to build from source but was not found on PATH"
    local ref
    ref="$(resolve_source_ref)"
    say "Building wizard from source (${WIZARD_REPO}@${ref}) ..."
    local src_dir="${TMP_DIR}/wizard-src"
    git clone --depth 1 --branch "$ref" \
        "https://github.com/${WIZARD_REPO}" "$src_dir" \
        || die "git clone failed — check WIZARD_REPO (${WIZARD_REPO}) and the ref (${ref})"
    ensure_rust_toolchain
    say "Running cargo build --release (this may take several minutes) ..."
    ( cd "$src_dir" && cargo build --release ) \
        || die "cargo build --release failed — see output above for details"
    local bin="${src_dir}/target/release/wizard"
    [ -f "$bin" ] \
        || die "build succeeded but target/release/wizard not found in ${src_dir}"
    "$bin" --version >/dev/null 2>&1 \
        || die "the built binary does not run ('wizard --version' failed) — the ${ref} ref may be broken"
    place_binary "$bin"
    BINARY_INSTALLED=1
    say "Installed wizard (built from source) to ${INSTALLED_PATH}"
}

# --- model selection (interactive) --------------------------------------

pull_model_tag() {
    # $1 = tag to pull
    command -v ollama >/dev/null 2>&1 \
        || die "ollama binary not found — cannot pull '$1'"
    say "Pulling $1 ..."
    ollama pull "$1" \
        || die "failed to pull '$1' — check the tag and your connectivity, then re-run"
}

choose_model() {
    if [ -n "$WIZARD_MODEL" ]; then
        MODEL="$WIZARD_MODEL"
        say "Model set via WIZARD_MODEL: ${MODEL} (no pull — make sure it is available in Ollama)"
        return
    fi

    require_tty

    printf '\n==> Wizard BYOM Setup\n\n' >/dev/tty
    printf 'Choose how to configure your model:\n\n' >/dev/tty
    printf '  1) Pull an existing Ollama library model\n' >/dev/tty
    printf '  2) Pull a custom Ollama registry tag\n' >/dev/tty
    printf '  3) Create from a local Modelfile\n' >/dev/tty
    printf '  4) Use a model already installed (skip pull)\n' >/dev/tty
    printf '\n' >/dev/tty

    local choice
    while true; do
        ask choice "Selection [1-4]: "
        case "$choice" in
            1 | 2 | 3 | 4) break ;;
            *) printf 'Please enter 1, 2, 3, or 4.\n' >/dev/tty ;;
        esac
    done

    case "$choice" in
        1)
            local tag
            while true; do
                ask tag "Enter Ollama library model (e.g. qwen3.6:27b, qwen3-coder:30b, deepseek-r1:32b): "
                [ -n "$tag" ] && break
            done
            pull_model_tag "$tag"
            MODEL="$tag"
            ;;
        2)
            local tag
            while true; do
                ask tag "Enter Ollama model tag (e.g. myuser/my-model:27b): "
                [ -n "$tag" ] && break
            done
            pull_model_tag "$tag"
            MODEL="$tag"
            say "Note: Wizard's agent loop works best with models that support tool calling."
            ;;
        3)
            command -v ollama >/dev/null 2>&1 \
                || die "ollama binary not found — cannot create a model from a Modelfile"
            local mf name
            while true; do
                ask mf "Path to Modelfile: "
                [ -z "$mf" ] && continue
                # Expand a leading ~ since the answer is not shell-expanded
                # (the literal ~ in the patterns is intentional: it is what
                # the user typed, not a path we expect the shell to expand).
                # shellcheck disable=SC2088
                case "$mf" in
                    "~/"*) mf="$HOME/${mf#\~/}" ;;
                    "~")   mf="$HOME" ;;
                esac
                if [ -f "$mf" ]; then
                    break
                fi
                printf 'No such file: %s\n' "$mf" >/dev/tty
            done
            while true; do
                ask name "Name for the new model (e.g. my-coder): "
                [ -n "$name" ] && break
            done
            say "Creating ${name} from ${mf} ..."
            ollama create "$name" -f "$mf" \
                || die "ollama create failed — check the Modelfile and try again"
            MODEL="$name"
            ;;
        4)
            if command -v ollama >/dev/null 2>&1; then
                printf '\nInstalled models:\n\n' >/dev/tty
                ollama list >/dev/tty 2>/dev/null || true
                printf '\n' >/dev/tty
            fi
            local name
            while true; do
                ask name "Model name to use: "
                [ -n "$name" ] && break
            done
            if command -v ollama >/dev/null 2>&1 \
                && ! ollama list 2>/dev/null | awk 'NR > 1 {print $1}' | grep -Fxq "$name"; then
                warn "'$name' does not appear in 'ollama list' — Wizard will fail at startup if it is missing"
            fi
            MODEL="$name"
            ;;
    esac
}

# --- config -------------------------------------------------------------

write_config() {
    local cfg="$HOME/.wizard/config.toml"
    mkdir -p "$HOME/.wizard"
    say "Writing ${cfg}"

    if [ -f "$cfg" ]; then
        # Don't clobber an existing config — only record the chosen model.
        if grep -qE '^[[:space:]]*model[[:space:]]*=' "$cfg"; then
            sed -i.wizard-bak -E "s|^[[:space:]]*model[[:space:]]*=.*|model = \"${MODEL}\"|" "$cfg"
            rm -f "${cfg}.wizard-bak"
        else
            printf 'model = "%s"\n' "$MODEL" >>"$cfg"
        fi
        say "Updated model = \"${MODEL}\" in existing config (other settings preserved)"
        return
    fi

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
    say "Wizard BYOM installer"
    require_curl
    detect_platform

    if [ "$WIZARD_BUILD_FROM_SOURCE" = "1" ]; then
        build_from_source
    else
        download_binary
        if [ "$BINARY_INSTALLED" != "1" ]; then
            say "No prebuilt binary found; falling back to building from source ..."
            build_from_source
        fi
    fi

    install_ollama
    start_ollama
    install_toolchain
    choose_model
    write_config

    printf '\n'
    if [ "$BINARY_INSTALLED" = "1" ]; then
        say "Done. Run: wizard"
    else
        say "Model configured, but the wizard binary was NOT installed — see the build-from-source steps above."
    fi
}

main "$@"
