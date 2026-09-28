#!/usr/bin/env bash
# Drive an installed Wizard GUI through a self-update against the fake feed
# from update-smoke-feed.sh: launch it, wait for the banner, start the update
# and then Restart to update from the command palette, and check that the
# app comes back as the new version. Screenshots and logs go to <out dir>.
#
# Usage: scripts/update-smoke-app.sh macos <"Wizard GUI.app"> <feed dir> <new version> <out dir>
#        scripts/update-smoke-app.sh linux <wizard-gui launcher> <feed dir> <new version> <out dir>
# Linux needs an X display (Xvfb), xdotool and ImageMagick's import.

set -uo pipefail

os="$1"
target="$2"
feed="$3"
new="$4"
out="$5"
mkdir -p "$out"
url="https://127.0.0.1:8443/releases"
data="$HOME/.zeron"

mkdir -p "$data"
# Skip onboarding so the sidebar and the command palette are up.
printf '{"onboardingCompleted": true}\n' >"$data/ui-settings.json"

log() { echo "[$(date +%T)] $*"; }

if [ "$os" = macos ]; then
  exe="$(defaults read "$target/Contents/Info" CFBundleExecutable)"
  shot() { screencapture -x "$out/$1.png" && log "shot $1"; }
  pids() { pgrep -x "$exe"; }
  focus() { osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $1) to true" >/dev/null 2>&1; }
  palette() {
    focus "$1"
    osascript -e 'tell application "System Events" to keystroke "k" using {command down}'
    sleep 1
    osascript -e "tell application \"System Events\" to keystroke \"$2\""
    sleep 1
    osascript -e 'tell application "System Events" to key code 36'
  }
  # PlistBuddy, not `defaults read`: cfprefsd caches plists by path.
  plist_version() { /usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$1/Contents/Info.plist"; }
  version_on_disk() { plist_version "$target"; }
  # Apps opened through Launch Services get launchd's environment, not ours.
  launchctl setenv WIZARD_GUI_RELEASES_URL "$url"
  launchctl setenv WIZARD_GUI_TEST_CA "$feed/ca.pem"
  open "$target"
else
  shot() {
    import -window root "$out/$1.png"
    # The window on its own too: the root capture comes back black for a
    # 32-bit window with no compositor.
    local window
    window="$(xdotool search --onlyvisible --class wizard-gui 2>/dev/null | head -1)"
    [ -n "$window" ] && import -window "$window" "$out/$1-window.png"
    log "shot $1"
  }
  pids() { pgrep -f '/wizard-gui$'; }
  focus() {
    local window
    window="$(xdotool search --pid "$1" --onlyvisible 2>/dev/null | head -1)"
    [ -n "$window" ] || window="$(xdotool search --onlyvisible --class wizard-gui 2>/dev/null | head -1)"
    [ -n "$window" ] || window="$(xdotool search --onlyvisible --name 'Wizard GUI' 2>/dev/null | head -1)"
    [ -n "$window" ] && xdotool windowactivate "$window" 2>/dev/null
    [ -n "$window" ] && xdotool windowfocus --sync "$window"
  }
  palette() {
    focus "$1"
    xdotool key ctrl+k
    sleep 1
    xdotool type --delay 30 "$2"
    sleep 1
    xdotool key Return
  }
  version_on_disk() { readlink "$HOME/.local/share/wizard-gui/current"; }
  WIZARD_GUI_RELEASES_URL="$url" WIZARD_GUI_TEST_CA="$feed/ca.pem" \
    nohup "$target" >"$out/stdout.log" 2>&1 &
fi

old_version="$(version_on_disk)"
log "installed version before: $old_version"
for _ in $(seq 60); do
  PID="$(pids | head -1)"
  [ -n "$PID" ] && break
  sleep 1
done
[ -n "${PID:-}" ] || { echo "::error::the app never started"; exit 1; }
log "running as pid $PID"

# The first check runs about 10s after the engine is up.
sleep 30
shot 01-banner

palette "$PID" "update wizard"
staged=""
for i in $(seq 120); do
  [ $((i % 5)) -eq 1 ] && shot "02-updating-$(printf %03d "$i")"
  if [ "$os" = macos ]; then
    [ -d "$data/updates/$new/$(basename "$target")" ] && staged=1
  else
    [ -x "$HOME/.local/share/wizard-gui/$new/wizard-gui" ] && staged=1
  fi
  [ -n "$staged" ] && break
  sleep 1
done
sleep 3
shot 03-ready
[ -n "$staged" ] || { echo "::error::the update was never staged"; cp -R "$data/logs" "$out/" 2>/dev/null; exit 1; }

palette "$PID" "restart to update"
for _ in $(seq 60); do
  kill -0 "$PID" 2>/dev/null || break
  sleep 1
done
kill -0 "$PID" 2>/dev/null && { echo "::error::the app did not quit for the restart"; exit 1; }
log "old instance exited"
NEW_PID=""
for _ in $(seq 90); do
  NEW_PID="$(pids | grep -vx "$PID" | head -1)"
  [ -n "$NEW_PID" ] && break
  sleep 1
done
[ -n "$NEW_PID" ] || { echo "::error::the app did not relaunch"; exit 1; }
log "relaunched as pid $NEW_PID"
now_version="$(version_on_disk)"
log "installed version after: $now_version"
sleep 20
shot 04-relaunched
palette "$NEW_PID" "check for updates"
sleep 8
shot 05-about

{
  echo "before: $old_version"
  echo "after: $now_version"
  if [ "$os" = macos ]; then
    echo "backup: $(plist_version "$(dirname "$target")/.$(basename "$target").previous" 2>&1)"
    "$target/Contents/MacOS/$exe" --version
  else
    ls -la "$HOME/.local/share/wizard-gui" "$HOME/.local/bin"
    echo "running: $(readlink "/proc/$NEW_PID/exe")"
    "$HOME/.local/bin/wizard-gui" --version
  fi
} | tee "$out/result.txt"
cp -R "$data/logs" "$out/" 2>/dev/null

status=0
[ "$now_version" = "$new" ] || { echo "::error::expected $new on disk, found $now_version"; status=1; }
if [ "$os" = macos ]; then
  osascript -e "tell application \"$(defaults read "$target/Contents/Info" CFBundleName)\" to quit" 2>/dev/null || kill "$NEW_PID"
  launchctl unsetenv WIZARD_GUI_RELEASES_URL
  launchctl unsetenv WIZARD_GUI_TEST_CA
else
  grep -q "/$new/wizard-gui" <<<"$(readlink "/proc/$NEW_PID/exe")" || { echo "::error::the relaunched process is not $new"; status=1; }
  kill "$NEW_PID" 2>/dev/null
fi
exit "$status"
