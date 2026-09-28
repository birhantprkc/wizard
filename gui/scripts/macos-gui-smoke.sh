#!/usr/bin/env bash
# Launch the packaged app on a macOS desktop session (GitHub's macOS runners
# have one), walk it with clicks and keystrokes, and screenshot every step.
#
# Usage: scripts/macos-gui-smoke.sh <"Wizard GUI.app"> <out dir>
# Env:   STEPS  newline-separated steps, each one of
#                 shot <name>              screenshot the screen
#                 click <fx> <fy> <name>   click at a fraction of the window, then shot
#                 clickc <dx> <dy> <name>  click <dx>,<dy> points from the window's
#                                          center (onboarding cards are centered)
#                 key <keys> <name>        osascript keystroke, e.g. "cmd+," then shot
#                 wait <seconds>
#                 burst <name> <count>     one shot per second
#               (default: a single launch screenshot)
#
# Also writes ax.txt (process, menu bar, window names and bounds, as System
# Events sees them) and copies the app log and any crash report.

set -uo pipefail

APP="$1"
OUT="$2"
mkdir -p "$OUT"
NAME="$(defaults read "$APP/Contents/Info" CFBundleName)"
EXE="$(defaults read "$APP/Contents/Info" CFBundleExecutable)"
brew list cliclick >/dev/null 2>&1 || brew install --quiet cliclick >/dev/null

shot() { screencapture -x "$OUT/$1.png" && echo "shot $1"; }

bounds() {
  osascript -e "tell application \"System Events\" to tell (first process whose unix id is $PID) to get {position, size} of window 1" 2>/dev/null | tr -d ' '
}

alive() {
  if ! kill -0 "$PID" 2>/dev/null; then
    echo "::error::the app is gone"
    return 1
  fi
}

open "$APP"
for _ in $(seq 60); do
  PID="$(pgrep -x "$EXE" | head -1)"
  [ -n "$PID" ] && break
  sleep 1
done
[ -n "${PID:-}" ] || { echo "::error::$EXE never started"; exit 1; }
sleep 12

{
  echo "bundle name: $NAME, executable: $EXE, pid: $PID"
  echo "--- foreground processes"
  osascript -e 'tell application "System Events" to get name of every process whose background only is false'
  echo "--- this process"
  osascript -e "tell application \"System Events\" to tell (first process whose unix id is $PID) to get {name, displayed name, frontmost}"
  echo "--- menu bar"
  osascript -e "tell application \"System Events\" to tell (first process whose unix id is $PID) to get name of every menu bar item of menu bar 1"
  echo "--- app menu"
  osascript -e "tell application \"System Events\" to tell (first process whose unix id is $PID) to get name of every menu item of menu 1 of menu bar item 2 of menu bar 1"
  echo "--- windows"
  osascript -e "tell application \"System Events\" to tell (first process whose unix id is $PID) to get {name, position, size} of every window"
  echo "--- appearance"
  defaults read -g AppleInterfaceStyle 2>&1
} >"$OUT/ax.txt" 2>&1
cat "$OUT/ax.txt"

shot 01-launch
osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $PID) to true" || true

failed=0
while read -r verb a b c; do
  [ -z "${verb:-}" ] && continue
  case "$verb" in
    shot) shot "$a" ;;
    wait) sleep "$a" ;;
    burst)
      for i in $(seq "$b"); do
        shot "$a-$(printf %02d "$i")"
        sleep 1
      done
      ;;
    click)
      IFS=, read -r x y w h <<<"$(bounds)"
      if [ -z "${w:-}" ]; then
        echo "::warning::no window bounds for click $c"
        continue
      fi
      px=$(python3 -c "print(int($x + $w * $a))")
      py=$(python3 -c "print(int($y + $h * $b))")
      cliclick "c:$px,$py"
      sleep 2
      shot "$c"
      ;;
    clickc)
      IFS=, read -r x y w h <<<"$(bounds)"
      if [ -z "${w:-}" ]; then
        echo "::warning::no window bounds for click $c"
        continue
      fi
      cliclick "c:$((x + w / 2 + a)),$((y + h / 2 + b))"
      sleep 2
      shot "$c"
      ;;
    key)
      mods=()
      key="$a"
      while [[ "$key" == *+* ]]; do
        mods+=("${key%%+*} down")
        key="${key#*+}"
      done
      using=""
      if [ ${#mods[@]} -gt 0 ]; then
        using=" using {$(printf '%s, ' "${mods[@]}" | sed 's/, $//; s/cmd/command/g; s/ctrl/control/g; s/alt/option/g')}"
      fi
      case "$key" in
        return) osascript -e "tell application \"System Events\" to key code 36$using" ;;
        escape) osascript -e "tell application \"System Events\" to key code 53$using" ;;
        *) osascript -e "tell application \"System Events\" to keystroke \"$key\"$using" ;;
      esac
      sleep 2
      shot "$b"
      ;;
  esac
  alive || { failed=1; break; }
done <<<"${STEPS:-}"

cp "$HOME/.zeron/logs/"*.log "$OUT/" 2>/dev/null || true
cp "$HOME/Library/Logs/DiagnosticReports/"*"$EXE"* "$OUT/" 2>/dev/null || true
if alive; then
  osascript -e "tell application \"$NAME\" to quit" 2>/dev/null || kill "$PID"
fi
exit "$failed"
