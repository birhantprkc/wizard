#!/usr/bin/env bash
# Install agents the way the app's Install button does, from the environment
# a Dock or desktop-launcher start gets: an empty HOME, launchd's PATH, no
# terminal. Each case runs `zeron install-agent` in its own fresh HOME and
# fails unless the agent is installed and its CLI runs from that same
# minimal environment plus the PATH the app gives agent processes.
#
# Usage: scripts/install-smoke.sh <zeron binary> <log dir> <case>...
# Cases: pi, pi-no-shell (login-shell probe disabled), pi-no-node (no Node.js
#        anywhere; the app downloads one), claude-code, wizard.
# Env:   HIDE_NODE="dir…" directories whose node/npm/npx the pi-no-node case
#        moves aside first (restored on exit). Needs write access, so CI runs
#        it with sudo available.

set -euo pipefail

ZERON="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
LOGS="$2"
shift 2
mkdir -p "$LOGS"
LAUNCHD_PATH=/usr/bin:/bin:/usr/sbin:/sbin

hidden=()
restore() {
  # macOS bash 3.2 calls an empty array unbound under set -u.
  for f in ${hidden[@]+"${hidden[@]}"}; do
    sudo mv "$f.smoke-hidden" "$f" 2>/dev/null || true
  done
}
trap restore EXIT

hide_node() {
  for dir in ${HIDE_NODE:-}; do
    for tool in node npm npx corepack; do
      if [ -e "$dir/$tool" ] || [ -L "$dir/$tool" ]; then
        sudo mv "$dir/$tool" "$dir/$tool.smoke-hidden"
        hidden+=("$dir/$tool")
      fi
    done
  done
  if env -i PATH="$LAUNCHD_PATH:/opt/homebrew/bin:/usr/local/bin" sh -c 'command -v node' >/dev/null; then
    echo "node is still reachable; set HIDE_NODE" >&2
    exit 1
  fi
}

run_case() {
  local name="$1" agent="$2" cli="$3"
  shift 3
  local home
  home="$(mktemp -d "${RUNNER_TEMP:-/tmp}/smoke-home-$name.XXXXXX")"
  local log="$LOGS/$name.log"
  echo "::group::$name ($agent)"
  local status=0
  # env -i: no SHELL, no TERM, no user PATH. What launchd hands a Dock app.
  env -i HOME="$home" PATH="$LAUNCHD_PATH" USER="${USER:-runner}" LOGNAME="${USER:-runner}" \
    TMPDIR="${TMPDIR:-/tmp}" "$@" "$ZERON" install-agent "$agent" </dev/null >"$log" 2>&1 || status=$?
  cat "$log"
  if [ "$status" -ne 0 ]; then
    echo "::error::$name: install-agent exited $status"
    echo "::endgroup::"
    return 1
  fi
  grep -q '^installed=true$' "$log"
  # The CLI must start from the PATH the app hands agents: the launchd one
  # plus the directories gui_path adds (and Pi's standalone Node, first).
  local bin
  bin="$(find "$home" -path "*/bin/$cli" \( -type f -o -type l \) | head -1)"
  local path="$LAUNCHD_PATH"
  [ -d "$home/.local/share/pi-node/current/bin" ] && path="$home/.local/share/pi-node/current/bin:$path"
  path="$path:/opt/homebrew/bin:/usr/local/bin"
  if [ -z "$bin" ]; then
    bin="$(env -i PATH="$path" sh -c "command -v $cli" || true)"
  fi
  echo "cli: $bin"
  env -i HOME="$home" PATH="$path" "$bin" --version | tee -a "$log"
  # Progress: the bar moved through more than one status and ended near done.
  local statuses
  statuses="$(grep -cE '^ *[0-9]+%' "$log" || true)"
  echo "progress lines: $statuses"
  [ "$statuses" -ge 3 ]
  echo "::endgroup::"
}

for case in "$@"; do
  case "$case" in
    pi) run_case pi pi pi ;;
    pi-no-shell) run_case pi-no-shell pi pi env ZERON_NO_LOGIN_SHELL=1 ;;
    pi-no-node)
      hide_node
      run_case pi-no-node pi pi env ZERON_NO_LOGIN_SHELL=1
      grep -q 'Downloading Node.js' "$LOGS/pi-no-node.log"
      restore
      hidden=()
      ;;
    claude-code) run_case claude-code claude-code claude ;;
    wizard) run_case wizard wizard wizard ;;
    *)
      echo "unknown case $case" >&2
      exit 2
      ;;
  esac
done
