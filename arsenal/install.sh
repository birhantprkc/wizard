#!/usr/bin/env bash
#
# Wizard Arsenal installer.
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard-arsenal/main/install.sh | bash
#
# Wizard Arsenal is upstream Wizard with a fuller default loadout. This script
# is a thin wrapper that:
#   1. Detects GPU VRAM (or system RAM) and selects a Qwen model tier
#      (same logic as upstream Wizard's installer).
#   2. Stages ~/.wizard/config.toml from the Arsenal template, substituting the
#      selected model — only if no config exists (never clobbers).
#   3. Runs upstream Wizard's installer from source, which installs Ollama,
#      pulls the model, and builds + installs the `wizard` binary.
#   4. Lays down the Arsenal configuration: ~/.wizard/mcp.toml (Playwright MCP)
#      and ~/.wizard/subagents/*.toml (reviewer, researcher, tester, documenter)
#      — each only if absent.
#
# The binary is plain upstream Wizard; Arsenal ships configuration, not source
# changes. Nothing under ~/.wizard/ that already exists is overwritten.
#
# Environment variables:
#   ARSENAL_REPO   owner/repo to fetch Arsenal config from (default teddytennant/wizard-arsenal)
#   ARSENAL_REF    git ref/branch for the Arsenal config    (default main)
#   WIZARD_REPO    upstream Wizard repo to build the binary  (default teddytennant/wizard)
#   WIZARD_REF     upstream Wizard ref to build              (default main)
#   WIZARD_MODEL   force a specific model tag                (default auto-detected)
#   (all other WIZARD_* vars from upstream install.sh are honored)

set -euo pipefail

# --- defaults -----------------------------------------------------------

ARSENAL_REPO="${ARSENAL_REPO:-teddytennant/wizard-arsenal}"
ARSENAL_REF="${ARSENAL_REF:-main}"
ARSENAL_RAW="https://raw.githubusercontent.com/${ARSENAL_REPO}/${ARSENAL_REF}"

UPSTREAM_REPO="${WIZARD_REPO:-teddytennant/wizard}"
UPSTREAM_REF="${WIZARD_REF:-main}"
UPSTREAM_INSTALL_URL="https://raw.githubusercontent.com/${UPSTREAM_REPO}/${UPSTREAM_REF}/install.sh"

WIZARD_MODEL="${WIZARD_MODEL:-}"

WIZARD_DIR="${HOME}/.wizard"
SUBAGENTS=(reviewer researcher tester documenter)

MODEL=""
MEM_GB=0
MEM_SOURCE=""

# --- output helpers -----------------------------------------------------

say()  { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

require_curl() {
    command -v curl >/dev/null 2>&1 || die "curl is required but was not found on PATH"
}

# --- model tier selection (copied verbatim from upstream install.sh) -----

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

# --- fetch helper -------------------------------------------------------

# fetch_to <url> <dest>: download url to dest, failing loudly. Writes via a
# temp file so a partial download never lands at dest.
fetch_to() {
    local url="$1" dest="$2" tmp
    tmp="$(mktemp)"
    if ! curl -fsSL -o "$tmp" "$url"; then
        rm -f "$tmp"
        die "failed to download ${url}"
    fi
    mv "$tmp" "$dest"
}

# --- staging the Arsenal config -----------------------------------------

stage_config() {
    local cfg="${WIZARD_DIR}/config.toml"
    mkdir -p "$WIZARD_DIR"
    if [ -f "$cfg" ]; then
        say "Existing config found at ${cfg} — leaving it untouched"
        say "To use the selected model, set in it: model = \"${MODEL}\""
        return
    fi
    say "Writing ${cfg} (model: ${MODEL})"
    local tmpl
    tmpl="$(mktemp)"
    fetch_to "${ARSENAL_RAW}/config/config.toml.template" "$tmpl"
    # Substitute the detected model for the __MODEL__ placeholder.
    sed "s|__MODEL__|${MODEL}|g" "$tmpl" >"$cfg"
    rm -f "$tmpl"
}

run_upstream_install() {
    say "Installing upstream Wizard (${UPSTREAM_REPO}@${UPSTREAM_REF}, build from source) ..."
    # Build from source so the binary works on any supported machine. The model
    # is forced to the one we staged into config.toml so the pull matches.
    # config.toml already exists at this point, so upstream leaves it untouched.
    curl -fsSL "$UPSTREAM_INSTALL_URL" \
        | WIZARD_REPO="$UPSTREAM_REPO" \
          WIZARD_REF="$UPSTREAM_REF" \
          WIZARD_BUILD_FROM_SOURCE=1 \
          WIZARD_MODEL="$MODEL" \
          bash \
        || die "upstream Wizard installer failed — see the output above"
}

stage_mcp() {
    local dest="${WIZARD_DIR}/mcp.toml"
    if [ -f "$dest" ]; then
        say "Existing ${dest} — leaving it untouched"
        return
    fi
    say "Writing ${dest} (Playwright MCP browser)"
    fetch_to "${ARSENAL_RAW}/config/mcp.toml" "$dest"
}

stage_subagents() {
    local dir="${WIZARD_DIR}/subagents" name dest
    mkdir -p "$dir"
    for name in "${SUBAGENTS[@]}"; do
        dest="${dir}/${name}.toml"
        if [ -f "$dest" ]; then
            say "Existing subagent ${dest} — leaving it untouched"
            continue
        fi
        say "Installing subagent: ${name}"
        fetch_to "${ARSENAL_RAW}/config/subagents/${name}.toml" "$dest"
    done
}

check_node() {
    if ! command -v npx >/dev/null 2>&1; then
        warn "Node/npx not found on PATH — the Playwright browser server will be skipped at startup."
        warn "Install Node (https://nodejs.org), then run /reload in Wizard to activate the browser."
    fi
}

# --- main ---------------------------------------------------------------

main() {
    say "Installing Wizard Arsenal — Wizard, batteries included"
    require_curl
    select_model
    stage_config
    run_upstream_install
    stage_mcp
    stage_subagents
    check_node

    printf '\n'
    say "Wizard Arsenal is ready."
    say "Configured: ${MODEL} (local Ollama), Playwright browser, subagents (${SUBAGENTS[*]})."
    say "Run: wizard"
}

main "$@"
