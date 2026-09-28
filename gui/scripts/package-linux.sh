#!/usr/bin/env bash
# Linux packaging: build the release binary and produce
#   target/package/wizard-gui-<version>-linux-<arch>.tar.gz
# containing the binary (as wizard-gui), the .desktop entry, and the icon, plus
# an install.sh that installs them under ~/.local/share/wizard-gui/<version>
# (the layout the in-app updater swaps) and links ~/.local/bin/wizard-gui.
#
# Usage: scripts/package-linux.sh
# Env:   PROFILE=debug for a fast unoptimized package (CI smoke); default release.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
PROFILE="${PROFILE:-release}"
ARCH="$(uname -m)"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
OUT_DIR="$ROOT/target/package"
STAGE="$OUT_DIR/wizard-gui-$VERSION-linux-$ARCH"
TARBALL="$STAGE.tar.gz"

cd "$ROOT"
if [[ "$PROFILE" == "release" ]]; then
  cargo build --release -p zeron
  BIN="$ROOT/target/release/zeron"
else
  cargo build -p zeron
  BIN="$ROOT/target/debug/zeron"
fi

rm -rf "$STAGE" "$TARBALL"
mkdir -p "$STAGE"
install -m 755 "$BIN" "$STAGE/wizard-gui"
install -m 644 "$ROOT/dist/wizard-gui.desktop" "$STAGE/wizard-gui.desktop"
install -m 644 "$ROOT/dist/wizard-gui.png" "$STAGE/wizard-gui.png"
mkdir -p "$STAGE/licenses/fonts"
cp "$ROOT/crates/ui/assets/fonts/licenses/"* "$STAGE/licenses/fonts/"

cat >"$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# Install Wizard GUI into ~/.local (no root needed). Each version gets its own
# directory under ~/.local/share/wizard-gui, with a `current` symlink that
# ~/.local/bin/wizard-gui points through. The in-app updater installs the next
# version the same way and keeps the previous one.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION="__VERSION__"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
ROOT="$DATA/wizard-gui"
APPS="$HOME/.local/share/applications"
ICONS="$HOME/.local/share/icons/hicolor/1024x1024/apps"
mkdir -p "$ROOT" "$HOME/.local/bin"
STAGE="$ROOT/.install-$VERSION-$$"
rm -rf "$STAGE"
cp -a "$HERE/." "$STAGE"
rm -rf "${ROOT:?}/$VERSION"
mv "$STAGE" "$ROOT/$VERSION"
ln -sfn "$VERSION" "$ROOT/.current-$$"
mv -Tf "$ROOT/.current-$$" "$ROOT/current"
# Replaces the plain copy older packages put here, too.
ln -sfn "$ROOT/current/wizard-gui" "$HOME/.local/bin/.wizard-gui-$$"
mv -Tf "$HOME/.local/bin/.wizard-gui-$$" "$HOME/.local/bin/wizard-gui"
install -Dm644 "$HERE/wizard-gui.desktop" "$APPS/wizard-gui.desktop"
install -Dm644 "$HERE/wizard-gui.png" "$ICONS/wizard-gui.png"
# Earlier packages installed the same app as zeron. Drop that launcher entry
# (only if it is ours) so the menu does not list Wizard GUI twice.
if [ -f "$APPS/zeron.desktop" ] && grep -q '^Name=Wizard GUI$' "$APPS/zeron.desktop"; then
  rm -f "$APPS/zeron.desktop" "$ICONS/zeron.png"
  if [ -e "$HOME/.local/bin/zeron" ]; then
    echo "An older copy is still at ~/.local/bin/zeron; remove it if nothing else uses it."
  fi
fi
command -v update-desktop-database >/dev/null 2>&1 \
  && update-desktop-database "$HOME/.local/share/applications" || true
echo "Installed Wizard GUI $VERSION. Open it from your app menu, or run wizard-gui (~/.local/bin must be on PATH)."
INSTALL
sed -i "s/__VERSION__/$VERSION/" "$STAGE/install.sh"
chmod 755 "$STAGE/install.sh"

tar -czf "$TARBALL" -C "$OUT_DIR" "$(basename "$STAGE")"
rm -rf "$STAGE"
echo "packaged: $TARBALL"
tar -tzf "$TARBALL"
