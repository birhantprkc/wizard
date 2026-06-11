#!/usr/bin/env bash
#
# Wizard BYOM (bring-your-own-model) installer — back-compat shim.
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install-byom.sh | bash
#
# The BYOM flow now lives in the main installer behind WIZARD_BYOM=1:
#
#   curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_BYOM=1 bash
#
# This shim only keeps the old URL working: it downloads install.sh and runs
# it with WIZARD_BYOM=1. All other WIZARD_* environment variables pass through
# to it unchanged — see the header of install.sh for the full list.
#
# Environment variables (shim-specific):
#   WIZARD_REPO          owner/repo to fetch install.sh from (default teddytennant/wizard)
#   WIZARD_INSTALLER_REF git ref to fetch install.sh from    (default main; independent
#                        of WIZARD_REF, which selects the source-build ref)

set -euo pipefail

WIZARD_REPO="${WIZARD_REPO:-teddytennant/wizard}"
WIZARD_INSTALLER_REF="${WIZARD_INSTALLER_REF:-main}"

INSTALL_URL="https://raw.githubusercontent.com/${WIZARD_REPO}/${WIZARD_INSTALLER_REF}/install.sh"

say() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required but was not found on PATH"

TMP_SCRIPT="$(mktemp)"
cleanup() { rm -f "$TMP_SCRIPT"; }
trap cleanup EXIT

say "install-byom.sh is now install.sh with WIZARD_BYOM=1 — fetching ${INSTALL_URL} ..."
if ! curl -fsSL -o "$TMP_SCRIPT" "$INSTALL_URL" 2>/dev/null; then
    # Unauthenticated raw URLs 404 on a private repo; an authenticated gh CLI
    # can still fetch the file (mirrors install.sh's release-asset fallback).
    if ! command -v gh >/dev/null 2>&1 \
        || ! gh api "repos/${WIZARD_REPO}/contents/install.sh?ref=${WIZARD_INSTALLER_REF}" \
            -H "Accept: application/vnd.github.raw" >"$TMP_SCRIPT" 2>/dev/null; then
        die "failed to download ${INSTALL_URL} — check connectivity (or 'gh auth login' if ${WIZARD_REPO} is private)"
    fi
fi

WIZARD_BYOM=1 bash "$TMP_SCRIPT" "$@"
