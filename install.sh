#!/usr/bin/env bash
#
# Wizard installer — one script, four flavors.
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | bash
#
# Default (no flags):
#   1. Detect OS and CPU architecture
#   2. Download the `wizard` binary from GitHub releases
#   3. Lay down the default loadout: ~/.wizard/mcp.toml (Playwright browser MCP)
#      and ~/.wizard/subagents/*.toml (reviewer, researcher, tester, documenter)
#      — each file only if absent, never overwriting
#
# No model, no model runtime, no config.toml: the first `wizard` run opens
# onboarding, which asks which provider to use. Picking "Local" there is one
# step — wizard detects your hardware, downloads a fitting GGUF, and installs
# and manages llama-server itself (or reuses an existing Ollama install).
#
# Flavors (mutually exclusive):
#   WIZARD_LOCAL=1    preinstall the local stack non-interactively (headless
#                     boxes, provisioning scripts; what the default used to do):
#                       1. Install llama.cpp's `llama-server` if absent, using
#                          the GPU: on NVIDIA it compiles a CUDA build (when
#                          nvcc is present); on other GPUs it installs a Vulkan
#                          loader and uses the prebuilt Vulkan build; CPU build
#                          otherwise. Upgrades an earlier CPU-only build in place
#                       2. Select a model tier based on available VRAM (or
#                          system RAM on CPU-only)
#                       3. Download the matching Qwen3 GGUF (Q4_K_M) from
#                          Hugging Face
#                       4. Write ~/.wizard/config.toml (never clobbers an
#                          existing one)
#                     No server is started here: wizard launches llama-server
#                     itself on first run. WIZARD_USE_OLLAMA=1 is the
#                     Ollama-based variant (install Ollama, start it, pull the
#                     auto-tiered model) and implies this flavor — no need to
#                     also set WIZARD_LOCAL.
#   WIZARD_BYOM=1     bring your own model — install Ollama if absent and pick
#                     any Ollama-compatible model interactively (library tag,
#                     custom registry tag, local Modelfile, or one already
#                     installed), then write the config. You choose the model:
#                     Wizard does not ship, endorse, or maintain third-party
#                     model weights; you are responsible for their licenses.
#   WIZARD_MINIMAL=1  binary only — like the default but also skips the
#                     loadout; the first `wizard` run starts onboarding
#
# Environment variables:
#   WIZARD_INSTALL_DIR           where to place the binary    (default /usr/local/bin;
#                                ~/.local/bin on NixOS)
#   WIZARD_LOCAL                 1 = preinstall the llama.cpp stack and a model
#                                    (see above)               (default 0)
#   WIZARD_MINIMAL               1 = minimal install (see above)        (default 0)
#   WIZARD_BYOM                  1 = bring-your-own-model install (see above)
#                                    (default 0)
#   WIZARD_BESPOKE               deprecated alias for WIZARD_MINIMAL
#   WIZARD_MODEL                 local flavors: force a specific model tier
#                                (default auto-detected; with WIZARD_BYOM=1:
#                                use this tag as-is and skip the interactive
#                                prompts)
#   WIZARD_SKIP_MODEL_PULL       1 = local flavors: skip the model download (default 0)
#   WIZARD_SKIP_LLAMACPP_INSTALL 1 = WIZARD_LOCAL: llama-server managed elsewhere (default 0)
#   WIZARD_LLAMACPP_NO_CUDA      1 = never compile a CUDA llama-server; use the
#                                    prebuilt Vulkan/CPU build instead (default 0)
#   WIZARD_USE_OLLAMA            1 = local flavor on Ollama instead of llama.cpp
#                                    (implies WIZARD_LOCAL)    (default 0)
#   WIZARD_SKIP_OLLAMA_INSTALL   1 = Ollama managed elsewhere (default 0)
#   WIZARD_WITH_TOOLCHAIN        1 = eagerly install a Rust toolchain for deep evolve (default 0)
#   WIZARD_REPO                  owner/repo to install from   (default teddytennant/wizard)
#   WIZARD_REF                   git ref/tag when building from source
#                                (default: latest release tag, falling back to
#                                main only when the repo has no release)
#   WIZARD_BUILD_FROM_SOURCE     1 = build from source instead of downloading a release (default 0)

set -euo pipefail

# --- NixOS detection ----------------------------------------------------
# Defined early so the install-dir default below can branch on it. NixOS is
# not an FHS distro: prebuilt glibc binaries can't find /lib64/ld-linux and
# /usr/local/bin isn't on PATH, so the installer selects the static musl
# asset and installs to ~/.local/bin instead.
is_nixos() {
    [ -f /etc/NIXOS ] && return 0
    [ -r /etc/os-release ] && grep -qiE '^ID=nixos' /etc/os-release
}

# --- defaults -----------------------------------------------------------

# /usr/local/bin is the right default on FHS distros, but not on NixOS (not on
# PATH, wrong place for an FHS binary). An explicit WIZARD_INSTALL_DIR override
# always wins; otherwise pick ~/.local/bin on NixOS, /usr/local/bin elsewhere.
if [ -z "${WIZARD_INSTALL_DIR:-}" ]; then
    if is_nixos; then
        WIZARD_INSTALL_DIR="$HOME/.local/bin"
    else
        WIZARD_INSTALL_DIR="/usr/local/bin"
    fi
fi
WIZARD_LOCAL="${WIZARD_LOCAL:-0}"
WIZARD_MINIMAL="${WIZARD_MINIMAL:-0}"
WIZARD_BYOM="${WIZARD_BYOM:-0}"
WIZARD_MODEL="${WIZARD_MODEL:-}"
WIZARD_SKIP_MODEL_PULL="${WIZARD_SKIP_MODEL_PULL:-0}"
WIZARD_SKIP_LLAMACPP_INSTALL="${WIZARD_SKIP_LLAMACPP_INSTALL:-0}"
WIZARD_LLAMACPP_NO_CUDA="${WIZARD_LLAMACPP_NO_CUDA:-0}"
WIZARD_USE_OLLAMA="${WIZARD_USE_OLLAMA:-0}"
WIZARD_SKIP_OLLAMA_INSTALL="${WIZARD_SKIP_OLLAMA_INSTALL:-0}"
WIZARD_WITH_TOOLCHAIN="${WIZARD_WITH_TOOLCHAIN:-0}"
WIZARD_REPO="${WIZARD_REPO:-teddytennant/wizard}"
WIZARD_REF="${WIZARD_REF:-}"
WIZARD_BUILD_FROM_SOURCE="${WIZARD_BUILD_FROM_SOURCE:-0}"

# WIZARD_BESPOKE is the old name for the minimal install; honored as a deprecated alias.
if [ "${WIZARD_BESPOKE:-0}" = "1" ]; then WIZARD_MINIMAL=1; fi

REPO="${WIZARD_REPO}"
RELEASE_BASE="https://github.com/${WIZARD_REPO}/releases/latest/download"
LLAMACPP_REPO="ggml-org/llama.cpp"
LLAMACPP_URL="http://127.0.0.1:11435"
LLAMA_BIN_DIR="$HOME/.wizard/bin"
MODELS_DIR="$HOME/.wizard/models"
OLLAMA_URL="http://127.0.0.1:11434"

OS=""
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
        Linux)  OS="linux" ;;
        Darwin) OS="macos" ;;
        *)
            die "unsupported operating system: $os (Wizard supports Linux and macOS)"
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

    say "Platform: ${OS}/${ARCH}"
}

require_curl() {
    command -v curl >/dev/null 2>&1 || die "curl is required but was not found on PATH"
}

nixos_banner() {
    printf '\n'
    say "NixOS detected."
    warn "The supported, idiomatic way to run Wizard on NixOS is Nix, not this script:"
    printf '\n' >&2
    printf '    nix run github:%s              # run without installing\n' "$WIZARD_REPO" >&2
    printf '    nix profile install github:%s  # add to your profile\n' "$WIZARD_REPO" >&2
    printf '    # or add the flake as an input to your system/home configuration\n' >&2
    printf '\n' >&2
    warn "Proceeding with a static musl binary instead → ${WIZARD_INSTALL_DIR}"
    warn "Set WIZARD_INSTALL_DIR to override the install location."
    printf '\n'
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

gpu_present() {
    case "$MEM_SOURCE" in
        "GPU VRAM"*) return 0 ;;
        *) return 1 ;;
    esac
}

# Best-effort: on a GPU box with no Vulkan loader, install one so the prebuilt
# Vulkan build of llama.cpp can actually use the GPU. The NVIDIA/AMD drivers
# ship the Vulkan ICD; only the loader (libvulkan) is usually missing — exactly
# the case on hosted notebooks (Colab) where the default install otherwise falls
# back to a CPU build and inference crawls. Never fatal: if no loader can be
# installed, the install proceeds on CPU with a warning.
ensure_vulkan_loader() {
    gpu_present || return 0
    have_vulkan_loader && return 0

    say "GPU detected but no Vulkan loader — installing one so llama.cpp can use the GPU ..."
    local sudo=""
    if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
        sudo="sudo"
    fi
    if command -v apt-get >/dev/null 2>&1; then
        $sudo apt-get update -qq >/dev/null 2>&1 || true
        $sudo apt-get install -y -qq libvulkan1 mesa-vulkan-drivers >/dev/null 2>&1 || true
    elif command -v dnf >/dev/null 2>&1; then
        $sudo dnf install -y vulkan-loader mesa-vulkan-drivers >/dev/null 2>&1 || true
    elif command -v yum >/dev/null 2>&1; then
        $sudo yum install -y vulkan-loader mesa-vulkan-drivers >/dev/null 2>&1 || true
    elif command -v pacman >/dev/null 2>&1; then
        $sudo pacman -Sy --noconfirm vulkan-icd-loader >/dev/null 2>&1 || true
    elif command -v zypper >/dev/null 2>&1; then
        $sudo zypper --non-interactive install libvulkan1 >/dev/null 2>&1 || true
    elif command -v apk >/dev/null 2>&1; then
        $sudo apk add --no-cache vulkan-loader >/dev/null 2>&1 || true
    fi

    if have_vulkan_loader; then
        say "Vulkan loader installed — using the GPU build of llama.cpp"
    else
        warn "could not install a Vulkan loader automatically — llama.cpp will run on CPU"
        warn "install one (e.g. 'apt-get install libvulkan1') and re-run for GPU acceleration"
    fi
}

# Whether the llama.cpp build Wizard installed is a GPU (Vulkan) build. Recorded
# in a .variant marker at install time; absent for installs predating it (and
# for external installs), which read as "not a GPU build" so a GPU box upgrades.
installed_llamacpp_is_gpu_build() {
    local marker="$HOME/.wizard/llama.cpp/.variant"
    [ -f "$marker" ] && grep -qE 'vulkan|cuda' "$marker"
}

# An NVIDIA GPU that nvidia-smi can see.
nvidia_gpu_present() {
    command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1
}

# Whether to build llama.cpp from source with CUDA. llama.cpp ships no prebuilt
# Linux CUDA binary, and the prebuilt Vulkan build cannot see an NVIDIA GPU on
# images that lack the Vulkan ICD (e.g. Colab) — so CUDA, compiled on the box,
# is the only reliable GPU path for NVIDIA. Requires nvcc already present (the
# full CUDA toolkit is multi-GB and not something an installer should pull).
should_build_cuda() {
    [ "$WIZARD_LLAMACPP_NO_CUDA" = "1" ] && return 1
    nvidia_gpu_present || return 1
    command -v nvcc >/dev/null 2>&1
}

# Best-effort install of the tools needed to compile llama.cpp (cmake, a C/C++
# compiler, git). Returns non-zero if any remain missing afterwards.
ensure_build_tools() {
    if ! { command -v cmake >/dev/null 2>&1 && command -v git >/dev/null 2>&1 \
        && { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; }; }; then
        local sudo=""
        if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
            sudo="sudo"
        fi
        if command -v apt-get >/dev/null 2>&1; then
            $sudo apt-get update -qq >/dev/null 2>&1 || true
            $sudo apt-get install -y -qq cmake build-essential git >/dev/null 2>&1 || true
        elif command -v dnf >/dev/null 2>&1; then
            $sudo dnf install -y cmake gcc gcc-c++ make git >/dev/null 2>&1 || true
        elif command -v pacman >/dev/null 2>&1; then
            $sudo pacman -Sy --noconfirm cmake base-devel git >/dev/null 2>&1 || true
        fi
    fi
    command -v cmake >/dev/null 2>&1 && command -v git >/dev/null 2>&1 \
        && { command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1; }
}

# Build llama-server from source with CUDA and install it under
# ~/.wizard/llama.cpp. Returns non-zero on any failure so the caller can fall
# back to the prebuilt Vulkan/CPU path.
build_llamacpp_cuda() {
    say "Building llama.cpp with CUDA for your NVIDIA GPU — this takes a few minutes ..."
    if ! ensure_build_tools; then
        warn "missing build tools (cmake/compiler/git) and could not install them — skipping CUDA build"
        return 1
    fi

    local src="${TMP_DIR}/llamacpp-cuda-src" jobs
    jobs="$(nproc 2>/dev/null || echo 4)"
    rm -rf "$src"
    if ! git clone --depth=1 https://github.com/"${LLAMACPP_REPO}".git "$src" >/dev/null 2>&1; then
        warn "could not clone llama.cpp for the CUDA build"
        return 1
    fi

    # Configure for the GPUs on this box (CMAKE_CUDA_ARCHITECTURES=native needs
    # CMake >= 3.24); retry without it on older CMake.
    if ! cmake -S "$src" -B "$src/build" -DCMAKE_BUILD_TYPE=Release \
        -DGGML_CUDA=ON -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF \
        -DCMAKE_CUDA_ARCHITECTURES=native >/dev/null 2>&1; then
        if ! cmake -S "$src" -B "$src/build" -DCMAKE_BUILD_TYPE=Release \
            -DGGML_CUDA=ON -DLLAMA_CURL=OFF -DLLAMA_BUILD_TESTS=OFF >/dev/null 2>&1; then
            warn "CUDA cmake configure failed (is the CUDA toolkit complete?)"
            return 1
        fi
    fi
    if ! cmake --build "$src/build" --config Release -j "$jobs" --target llama-server >/dev/null 2>&1; then
        warn "CUDA build failed"
        return 1
    fi

    local bin
    bin="$(find "$src/build" -type f -name llama-server | head -n1 || true)"
    if [ -z "$bin" ] || ! "$bin" --version >/dev/null 2>&1; then
        warn "the CUDA-built llama-server did not run"
        return 1
    fi

    local dest="$HOME/.wizard/llama.cpp"
    rm -rf "$dest"
    mkdir -p "$dest"
    # Keep the whole bin/ tree: the build's shared libraries (libggml-cuda.so,
    # …) must sit beside llama-server for its $ORIGIN runpath to resolve them.
    cp -R "$(dirname "$bin")"/. "$dest/"
    printf 'cuda-source\n' >"${dest}/.variant"
    mkdir -p "$LLAMA_BIN_DIR"
    ln -sfn "${dest}/llama-server" "${LLAMA_BIN_DIR}/llama-server"
    say "Installed CUDA llama-server to ${dest}"
    return 0
}

llamacpp_variants() {
    # Candidate release-asset variants, preferred first. llama.cpp ships no
    # Linux CUDA release asset; Vulkan is the prebuilt GPU backend (and it
    # falls back to CPU at runtime when no usable GPU is present), so try it
    # when a GPU and a Vulkan loader were detected, with plain CPU as the
    # safe fallback.
    local suffix="x64"
    [ "$ARCH" = "aarch64" ] && suffix="arm64"
    # macOS ships a single per-arch build with the Metal backend baked in.
    if [ "$OS" = "macos" ]; then
        printf 'macos-%s\n' "$suffix"
        return
    fi
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
    # On NixOS, never compile from source or drop a prebuilt FHS binary — use an
    # existing llama-server if present, otherwise point the user at Nix.
    if is_nixos; then
        if command -v llama-server >/dev/null 2>&1; then
            say "llama-server already on PATH ($(command -v llama-server)) — wizard will use it"
        else
            warn "On NixOS, install llama.cpp declaratively instead of compiling it here:"
            warn "    nix profile install nixpkgs#llama-cpp"
            warn "then re-run (or add it to your system/home configuration). Skipping for now."
        fi
        return
    fi
    # Decide the GPU strategy up front (needs to know whether a GPU is present).
    # NVIDIA → compile a CUDA build (the only reliable NVIDIA path; no prebuilt
    # CUDA asset exists and the Vulkan prebuilt can't see NVIDIA without an ICD).
    # Otherwise, on a GPU box, install a Vulkan loader and use the Vulkan
    # prebuilt. The strategy is non-empty only when a GPU build is actually
    # achievable, so the upgrade check below never churns on un-accelerable boxes.
    [ -n "$MEM_SOURCE" ] || detect_memory
    local gpu_strategy=""
    if gpu_present; then
        if should_build_cuda; then
            gpu_strategy="cuda"
        else
            ensure_vulkan_loader
            have_vulkan_loader && gpu_strategy="vulkan"
        fi
    fi

    # An existing install Wizard manages lives under ~/.wizard/llama.cpp. Upgrade
    # a CPU-only build to a GPU build when one is achievable; otherwise leave it.
    if [ -x "$HOME/.wizard/llama.cpp/llama-server" ]; then
        if [ -n "$gpu_strategy" ] && ! installed_llamacpp_is_gpu_build; then
            say "GPU detected but the installed llama-server is a CPU build — reinstalling a GPU build (${gpu_strategy})"
        else
            say "llama-server already installed at $HOME/.wizard/llama.cpp/llama-server"
            expose_llama_server
            return
        fi
    elif command -v llama-server >/dev/null 2>&1; then
        # An external llama-server (brew, nix, hand-built): never clobber it.
        say "llama-server already installed ($(command -v llama-server)) — leaving it as is"
        return
    fi

    # NVIDIA: compile CUDA; on success we're done, otherwise fall back to prebuilt.
    if [ "$gpu_strategy" = "cuda" ]; then
        if build_llamacpp_cuda; then
            expose_llama_server
            return
        fi
        warn "falling back to a prebuilt llama-server (Vulkan/CPU)"
        ensure_vulkan_loader
    fi

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
        # Record the variant so a later run knows whether this is a GPU build.
        printf '%s\n' "$variant" >"${dest}/.variant"
        mkdir -p "$LLAMA_BIN_DIR"
        ln -sfn "${dest}/llama-server" "${LLAMA_BIN_DIR}/llama-server"
        say "Installed llama-server to ${dest} (${variant} build)"
        expose_llama_server
        return
    done

    warn "could not install a prebuilt llama-server for ${OS}/${ARCH}"
    warn "install it yourself — wizard will start it automatically once it is on PATH:"
    printf '\n' >&2
    printf '    brew install llama.cpp                  # Homebrew / Linuxbrew\n' >&2
    printf '    nix profile install nixpkgs#llama-cpp   # Nix / NixOS\n' >&2
    printf '    https://github.com/%s — build from source\n' "$LLAMACPP_REPO" >&2
    printf '\n' >&2
}

# --- ollama (WIZARD_USE_OLLAMA=1 or WIZARD_BYOM=1) ------------------------

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
    # On NixOS the curl|sh Ollama installer drops an FHS binary that won't run —
    # require a declarative install instead.
    if is_nixos; then
        die "On NixOS, install Ollama declaratively rather than via the curl installer — e.g. 'nix profile install nixpkgs#ollama' (or set services.ollama.enable = true), then re-run."
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

    # macOS: Apple Silicon shares unified memory between CPU and the Metal GPU,
    # so total RAM is the right tiering signal (and the Metal-backed llama-server
    # can address most of it). sysctl reports it in bytes.
    if [ "$OS" = "macos" ]; then
        local mem_b
        mem_b="$(sysctl -n hw.memsize 2>/dev/null || true)"
        if is_uint "$mem_b" && [ "$mem_b" -gt 0 ]; then
            MEM_GB=$((mem_b / 1024 / 1024 / 1024))
            MEM_SOURCE="unified memory (Apple Silicon)"
            return
        fi
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

# --- BYOM model selection (WIZARD_BYOM=1, interactive) --------------------

pull_model_tag() {
    # $1 = tag to pull
    command -v ollama >/dev/null 2>&1 \
        || die "ollama binary not found — cannot pull '$1'"
    say "Pulling $1 ..."
    ollama pull "$1" \
        || die "failed to pull '$1' — check the tag and your connectivity, then re-run"
}

choose_byom_model() {
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
    # sha256sum on Linux; macOS ships `shasum -a 256` instead.
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tarball" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        actual="$(shasum -a 256 "$tarball" | awk '{print $1}')"
    else
        warn "no sha256 tool (sha256sum/shasum) found on PATH — skipping checksum verification"
        return
    fi
    if [ "$actual" != "$expected" ]; then
        die "checksum mismatch for ${asset} (expected ${expected}, got ${actual}) — the download may be corrupted or tampered with; aborting"
    fi
    say "Checksum verified for ${asset}"
}

download_binary() {
    say "Downloading wizard binary from GitHub releases (${REPO}) ..."
    local asset bin assets
    # macOS ships a single per-arch Mach-O asset. On Linux, NixOS can't run the
    # glibc (gnu) binary — no dynamic loader at the FHS path — so prefer the
    # static musl asset there. Elsewhere try gnu first but keep musl as a
    # fallback: if the gnu binary fails its sanity check (loader/glibc mismatch
    # on an old or unusual host), the loop drops to the static musl build.
    if [ "$OS" = "macos" ]; then
        assets="wizard-${ARCH}-apple-darwin.tar.gz"
    elif is_nixos; then
        assets="wizard-${ARCH}-unknown-linux-musl.tar.gz wizard-${ARCH}-unknown-linux-gnu.tar.gz wizard-linux-${ARCH}.tar.gz"
    else
        assets="wizard-${ARCH}-unknown-linux-gnu.tar.gz wizard-${ARCH}-unknown-linux-musl.tar.gz wizard-linux-${ARCH}.tar.gz"
    fi
    for asset in $assets; do
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

    warn "could not download a prebuilt wizard binary for ${OS}/${ARCH}"
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

# --- config -------------------------------------------------------------

write_config() {
    local cfg="$HOME/.wizard/config.toml"
    mkdir -p "$HOME/.wizard"
    # Only the local and BYOM flavors write a config. The default and minimal
    # flavors leave it to onboarding on the first `wizard` run.
    if [ "$WIZARD_MINIMAL" = "1" ] \
        || { [ "$WIZARD_LOCAL" != "1" ] && [ "$WIZARD_USE_OLLAMA" != "1" ] \
            && [ "$WIZARD_BYOM" != "1" ]; }; then
        if [ -f "$cfg" ]; then
            say "A config already exists at ${cfg} — leaving it untouched"
            say "Run 'wizard --onboard' to reconfigure from scratch"
        else
            say "No config written — the first 'wizard' run starts onboarding"
        fi
        return
    fi
    if [ -f "$cfg" ]; then
        if [ "$WIZARD_BYOM" = "1" ]; then
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
        say "Existing config found at ${cfg} — leaving it untouched"
        say "To switch models or providers, edit it or use /provider inside wizard"
        return
    fi
    say "Writing ${cfg}"
    if [ "$WIZARD_USE_OLLAMA" = "1" ] || [ "$WIZARD_BYOM" = "1" ]; then
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

# --- default loadout ------------------------------------------------------
# Browser MCP + subagent roster, written into ~/.wizard/. The canonical
# source for these files is the repo's loadout/ directory (loadout/mcp.toml,
# loadout/subagents/*.toml); they are embedded here as verbatim heredocs so
# the curl|bash one-liner works without a repo checkout. When you change one
# side, change the other — keep the two in sync.

loadout_file() {
    # $1 = destination path, $2 = short label; the file body arrives on
    # stdin (heredoc). Never overwrites: an existing file always wins.
    local dest="$1" label="$2"
    if [ -f "$dest" ]; then
        say "Existing ${dest} — leaving it untouched"
        return
    fi
    cat >"$dest"
    say "Installed ${label} (${dest})"
}

install_loadout() {
    if [ "$WIZARD_MINIMAL" = "1" ]; then
        say "Minimal install: skipping the default loadout (browser MCP, subagents)"
        return
    fi
    say "Laying down the default loadout (browser MCP + subagents) ..."
    mkdir -p "$HOME/.wizard/subagents"

    loadout_file "$HOME/.wizard/mcp.toml" "MCP servers: Playwright browser" <<'EOF'
# Wizard MCP server declarations — installed to ~/.wizard/mcp.toml
#
# Part of Wizard's default loadout. This directory (loadout/) is the canonical
# source; install.sh embeds a verbatim copy as a heredoc so the curl|bash
# one-liner works without a repo checkout — keep the two in sync.
#
# Each [[server]] is a Model Context Protocol server whose tools merge into
# Wizard's tool registry. New servers (or edits here) become active the next
# time Wizard starts, or immediately when you run /reload in the TUI.
#
# The Playwright MCP server below gives Wizard a real browser: navigate, click,
# type, and snapshot tools for reading pages, filling forms, and computer-use
# style tasks. It is spawned over stdio as `npx -y @playwright/mcp@latest`, so
# it requires Node and `npx` on your PATH. If Node is missing, this server is
# skipped with a warning at startup and the rest of Wizard works normally —
# install Node, then `/reload`.

[[server]]
name = "playwright"
transport = "stdio"
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
EOF

    loadout_file "$HOME/.wizard/subagents/reviewer.toml" "subagent: reviewer" <<'EOF'
name = "reviewer"
description = "Code-review specialist. Reads a diff or set of files and reports correctness bugs, security issues, and style problems. Read-only: never edits, runs, or commits anything."

# Read/search/git tools only — the reviewer inspects, it does not change code.
tool_scope = ["read_file", "list_files", "search_files", "git_status", "git_diff"]

max_steps = 20

system_prompt = """
You are the reviewer subagent of Wizard, a local agent. Your one job is
to review code and report findings. You cannot and must not edit, run, or
commit anything — you only have read, search, and git-inspection tools.

Method:
1. Establish scope. If reviewing changes, run `git_diff` (and `git_status` for
   untracked files) to see exactly what changed. If reviewing specific files,
   read them. Read enough surrounding context to judge each change correctly —
   a diff hunk in isolation lies.
2. Look, in priority order, for:
   - Correctness bugs: wrong logic, off-by-one, unhandled error/None/null
     paths, race conditions, resource leaks, broken invariants.
   - Security issues: injection, unvalidated input, secrets in code, unsafe
     deserialization, path traversal, missing authz checks.
   - API/contract breaks: changed signatures, behavior that callers depend on.
   - Tests: are the changes covered? Do existing tests still hold?
   - Clarity and style: naming, dead code, duplication, missing error context —
     reported last and clearly marked as lower priority.
3. For each finding give: file and line, severity (blocker / should-fix /
   nit), what is wrong, and a concrete suggested fix.

Be specific and honest. Do not invent problems to seem thorough; if the change
is clean, say so. Do not rubber-stamp; if you are unsure whether something is a
bug, say what would confirm it. End with a short verdict: APPROVE,
APPROVE-WITH-NITS, or REQUEST-CHANGES, followed by the findings list.
"""
EOF

    loadout_file "$HOME/.wizard/subagents/researcher.toml" "subagent: researcher" <<'EOF'
name = "researcher"
description = "Web research specialist. Uses the Playwright browser (MCP) to read pages, follow links, and gather facts, then reports a concise sourced summary. Use for questions that need current, external information."

# No tool_scope: the researcher gets the parent's full tool set, which includes
# the Playwright browser MCP tools (navigate / click / type / snapshot) shipped
# in mcp.toml. Without scope it can reach those browser tools.

max_steps = 25

system_prompt = """
You are the researcher subagent of Wizard, a local agent. Your job is to
answer a question using information from the web and report back. You have a
real browser available through the Playwright MCP tools (navigate, click, type,
snapshot, and related). Use them — do not claim you cannot browse.

Method:
1. Plan a couple of search angles for the question. Navigate to a search engine
   or directly to a likely-authoritative source (official docs, release notes,
   the project's own repo) and read the page via a snapshot.
2. Follow links and open additional pages as needed. Prefer primary sources
   (official documentation, source repositories, standards) over blog
   recaps. Cross-check anything surprising against a second source.
3. Extract the specific facts that answer the question. Note version numbers,
   dates, and exact quotes where precision matters.

If the browser tools are unavailable (Node/npx not installed, server failed to
start), say so plainly and report whatever you could determine from your own
knowledge, clearly labeled as un-verified — do not fabricate page contents or
citations.

Report concisely: lead with the direct answer, then the supporting findings,
then the URLs you actually visited as sources. Distinguish what you confirmed
from a source versus what you inferred. Never invent a source or a quote.
"""
EOF

    loadout_file "$HOME/.wizard/subagents/tester.toml" "subagent: tester" <<'EOF'
name = "tester"
description = "Test specialist. Runs the project's test suite, diagnoses failures, and fixes them — editing code or tests as appropriate — until the suite passes or the failure is clearly explained."

# Can read, search, edit/write, and run commands. No git tools: the tester
# fixes and verifies; committing is the parent's decision.
tool_scope = ["read_file", "write_file", "edit_file", "list_files", "search_files", "execute"]

max_steps = 30

system_prompt = """
You are the tester subagent of Wizard, a local agent. Your job is to get
the project's tests passing — or to explain precisely why they cannot pass.

Method:
1. Discover how this project is tested. Look for the build/test commands in
   AGENTS.md, WIZARD.md, README, or the manifest (Cargo.toml, package.json,
   pyproject.toml, Makefile, etc.). Common commands: `cargo test`, `npm test`,
   `pytest`, `go test ./...`, `make test`.
2. Run the suite with `execute` and read the full output. Identify the first
   real failure (compile/lint errors before test assertions).
3. Diagnose the root cause by reading the failing test and the code under test.
   Decide whether the bug is in the implementation or in the test itself, and
   fix the correct one. Do not delete or weaken a test to make it pass, and do
   not assert behavior the code never promised — fix the real defect.
4. Re-run the suite after each change. Iterate until it is green.

Rules:
- Make the smallest change that correctly fixes the failure.
- Never fabricate a passing result. If the suite still fails, report the exact
  failing tests and error output, your diagnosis, and what you changed.
- If a failure is environmental (missing dependency, no network, missing
  toolchain) and you cannot resolve it, say so explicitly rather than masking it.

Report: the command you ran, the final pass/fail state with counts, what you
changed and why, and any failures left unresolved with their cause.
"""
EOF

    loadout_file "$HOME/.wizard/subagents/documenter.toml" "subagent: documenter" <<'EOF'
name = "documenter"
description = "Documentation specialist. Writes and updates READMEs, docs pages, and code comments so they accurately match the code. Edits prose and docs, never application logic."

# Read/search to understand the code, edit/write to produce docs. No execute or
# git: the documenter writes documentation, it does not run or commit code.
tool_scope = ["read_file", "write_file", "edit_file", "list_files", "search_files"]

max_steps = 25

system_prompt = """
You are the documenter subagent of Wizard, a local agent. Your job is to
produce documentation that is accurate, clear, and matched to the actual code —
READMEs, docs pages, usage examples, and doc comments.

Method:
1. Read the relevant code and any existing docs before writing a word. Your
   documentation must describe what the code actually does, not what it ought
   to do. Trace function signatures, public APIs, config keys, CLI flags, and
   defaults to their source.
2. Match the existing documentation's voice, structure, and formatting. Reuse
   established headings and conventions; do not invent a new style.
3. Write for the reader: lead with what the thing is and how to use it, then
   details. Prefer concrete, runnable examples over abstract description.

Rules:
- Never document behavior you have not verified in the source. Do not invent
  flags, options, return values, or benchmarks. If something is ambiguous, note
  the ambiguity rather than guessing.
- Keep examples correct and minimal — every command or snippet you show should
  actually work as written.
- Edit only documentation and comments. Do not change application logic; if you
  notice a code bug while documenting, report it rather than fixing it.

Report: which files you wrote or updated, and a one-line summary of each change.
"""
EOF

    if ! command -v npx >/dev/null 2>&1; then
        warn "Node/npx not found on PATH — the Playwright browser server will be skipped at startup."
        warn "Install Node (https://nodejs.org), then run /reload in Wizard to activate the browser."
    fi
}

# --- main ---------------------------------------------------------------

main() {
    say "Wizard installer"
    if [ "$WIZARD_MINIMAL" = "1" ] && [ "$WIZARD_BYOM" = "1" ]; then
        die "WIZARD_MINIMAL=1 and WIZARD_BYOM=1 conflict — pick one: minimal installs the binary only (onboarding on first run), BYOM sets up Ollama with a model of your choice"
    fi
    if [ "$WIZARD_LOCAL" = "1" ] && [ "$WIZARD_MINIMAL" = "1" ]; then
        die "WIZARD_LOCAL=1 and WIZARD_MINIMAL=1 conflict — pick one: local preinstalls llama.cpp and an auto-tiered model, minimal installs the binary only (onboarding on first run)"
    fi
    if [ "$WIZARD_LOCAL" = "1" ] && [ "$WIZARD_BYOM" = "1" ]; then
        die "WIZARD_LOCAL=1 and WIZARD_BYOM=1 conflict — pick one: local preinstalls llama.cpp with an auto-tiered model, BYOM sets up Ollama with a model of your choice"
    fi
    require_curl
    detect_platform

    if is_nixos; then
        nixos_banner
    fi

    if [ "$WIZARD_MINIMAL" = "1" ]; then
        say "Minimal install (WIZARD_MINIMAL=1): binary only — no model runtime, model, config, or loadout"
    elif [ "$WIZARD_BYOM" = "1" ]; then
        say "BYOM install (WIZARD_BYOM=1): bring your own Ollama model"
        install_ollama
        start_ollama
        choose_byom_model
    elif [ "$WIZARD_USE_OLLAMA" = "1" ]; then
        say "Using Ollama as the local provider (WIZARD_USE_OLLAMA=1)"
        install_ollama
        start_ollama
        select_model
        pull_model
    elif [ "$WIZARD_LOCAL" = "1" ]; then
        say "Local install (WIZARD_LOCAL=1): llama.cpp runtime + hardware-tiered model"
        install_llamacpp
        select_model
        download_gguf
    else
        say "Default install: binary + loadout — pick a provider in onboarding on first run"
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
    install_loadout

    printf '\n'
    if [ "$BINARY_INSTALLED" = "1" ]; then
        if [ "$WIZARD_MINIMAL" = "1" ]; then
            say "Done. Run 'wizard' to start onboarding (pick your model, provider, and gateway)."
        elif [ "$WIZARD_BYOM" = "1" ] || [ "$WIZARD_USE_OLLAMA" = "1" ]; then
            say "Done. Run: wizard"
        elif [ "$WIZARD_LOCAL" = "1" ]; then
            say "Done. Run: wizard — it starts llama-server with your model automatically."
        else
            say "Done. Run 'wizard' — it asks which provider to use (Local is one pick: it downloads a model sized to your hardware and sets up llama.cpp for you)."
        fi
    else
        say "Setup finished, but the wizard binary was NOT installed — see the build-from-source steps above."
    fi
}

main "$@"
