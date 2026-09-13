#!/usr/bin/env bash
# What the completion review costs, and what it catches. No model is called:
# bench/review-mock-server.py replays a fixed script, so both the numbers and
# the transcript below are reproducible.
#
#   bash bench/completion-review.sh [path/to/wizard]
#
# First the overhead, as the same work run with the review off and on at three
# run lengths. Then one run where the agent claims a file it never wrote.
set -euo pipefail

WIZARD=${1:-target/release/wizard}
[ -x "$WIZARD" ] || { echo "no wizard binary at $WIZARD; cargo build --release" >&2; exit 1; }
WIZARD=$(cd "$(dirname "$WIZARD")" && pwd)/$(basename "$WIZARD")
HERE=$(cd "$(dirname "$0")" && pwd)
PORT=${PORT:-8099}
ROOT=$(mktemp -d /tmp/wizard-review-bench.XXXXXX)
DELIVERABLE=/tmp/wizard-review-demo/report.txt
REQUEST="Summarize this project into $DELIVERABLE. Check it with: test -s $DELIVERABLE"

server=""
cleanup() {
    [ -n "$server" ] && kill "$server" 2>/dev/null
    rm -rf "$ROOT" /tmp/wizard-review-demo
}
trap cleanup EXIT

start_server() { # $1 = calls log, $2... = mock flags
    local log=$1
    shift
    python3 "$HERE/review-mock-server.py" "$PORT" "$log" "$@" &
    server=$!
    for _ in $(seq 50); do
        curl -sf "http://127.0.0.1:$PORT/v1/models" >/dev/null && return
        sleep 0.1
    done
    echo "mock server did not come up" >&2
    exit 1
}

stop_server() {
    kill "$server" 2>/dev/null || true
    wait "$server" 2>/dev/null || true
    server=""
}

run() { # $1 = fake home, $2... = wizard flags
    local home=$1
    shift
    mkdir -p "$home/.wizard" "$home/work"
    cat >"$home/.wizard/config.toml" <<TOML
model = "mock-model"
mode = "sovereign"
active_provider = "mock"

[[providers]]
name = "mock"
kind = "openai"
base_url = "http://127.0.0.1:$PORT/v1"
model = "mock-model"
api_key_env = "MOCK_API_KEY"
TOML
    (cd "$home/work" && HOME=$home MOCK_API_KEY=mock "$WIZARD" -p "$REQUEST" "$@" 2>&1)
}

echo "=== overhead: the same work, review off then on ==="
echo "  The mock answers instantly, so wall time is not the cost here; model"
echo "  calls and prompt size are. 'steps' is how many tool calls the run makes."
printf '  %-7s %-24s %-28s %s\n' steps "model calls" "prompt chars" "the review itself"
for steps in 4 10 20; do
    rm -rf /tmp/wizard-review-demo
    start_server "$ROOT/off-$steps.jsonl" --honest "--steps=$steps"
    run "$ROOT/home-off-$steps" --no-completion-review >/dev/null
    stop_server

    rm -rf /tmp/wizard-review-demo
    start_server "$ROOT/on-$steps.jsonl" --honest "--steps=$steps"
    run "$ROOT/home-on-$steps" --completion-review >/dev/null
    stop_server

    python3 "$HERE/review-report.py" "$steps" "$ROOT/off-$steps.jsonl" "$ROOT/on-$steps.jsonl"
done

echo
echo "=== what it catches: the run claims a file it never wrote ==="
rm -rf /tmp/wizard-review-demo
mkdir -p /tmp/wizard-review-demo
start_server "$ROOT/catch.jsonl"
run "$ROOT/home-catch" --completion-review
stop_server
echo
echo "--- $DELIVERABLE after the run ---"
ls -l "$DELIVERABLE" 2>&1 || true
