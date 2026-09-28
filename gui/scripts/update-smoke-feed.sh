#!/usr/bin/env bash
# A fake Wizard GUI release feed for the self-update smoke test: a throwaway
# minisign key the test build trusts (WIZARD_GUI_TEST_RELEASE_KEY), a local
# CA and certificate for 127.0.0.1 (WIZARD_GUI_TEST_CA), and an HTTPS server
# laid out like GitHub's releases/latest/download and releases/download/v<ver>.
#
# Usage: scripts/update-smoke-feed.sh keys <dir>
#        scripts/update-smoke-feed.sh publish <dir> <version> <asset>...
#        scripts/update-smoke-feed.sh serve <dir>
# Env:   KEY=<name> signs with <dir>/<name>.key instead of test.key (publish).
# Needs minisign, openssl and python3.

set -euo pipefail

cmd="$1"
dir="$(mkdir -p "$2" && cd "$2" && pwd)"
shift 2

case "$cmd" in
keys)
  for name in test other; do
    [ -f "$dir/$name.key" ] || minisign -G -W -p "$dir/$name.pub" -s "$dir/$name.key" >/dev/null
  done
  openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=wizard-gui smoke CA" \
    -keyout "$dir/ca.key" -out "$dir/ca.pem" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign" 2>/dev/null
  openssl req -newkey rsa:2048 -nodes -subj "/CN=127.0.0.1" \
    -keyout "$dir/server.key" -out "$dir/server.csr" 2>/dev/null
  printf 'subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >"$dir/server.ext"
  openssl x509 -req -in "$dir/server.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" \
    -CAcreateserial -days 2 -extfile "$dir/server.ext" -out "$dir/server.pem" 2>/dev/null
  echo "keys in $dir: test.pub (trusted), other.pub (not), ca.pem"
  ;;
publish)
  version="$1"
  shift
  feed="$dir/feed/releases"
  rm -rf "$feed"
  mkdir -p "$feed/latest/download" "$feed/download/v$version"
  for asset in "$@"; do
    cp "$asset" "$feed/download/v$version/"
  done
  (cd "$feed/download/v$version" && shasum -a 256 -- * >"$feed/latest/download/gui-checksums.txt")
  minisign -S -s "$dir/${KEY:-test}.key" -m "$feed/latest/download/gui-checksums.txt" \
    -t "wizard v$version gui checksums, signed by the smoke test key" </dev/null >/dev/null
  cat "$feed/latest/download/gui-checksums.txt"
  ;;
serve)
  cat >"$dir/serve.py" <<'PY'
import functools, http.server, ssl, sys
root = sys.argv[1]
handler = functools.partial(http.server.SimpleHTTPRequestHandler, directory=root + "/feed")
server = http.server.ThreadingHTTPServer(("127.0.0.1", 8443), handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(root + "/server.pem", root + "/server.key")
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
PY
  nohup python3 "$dir/serve.py" "$dir" >"$dir/server.log" 2>&1 &
  echo $! >"$dir/server.pid"
  for _ in $(seq 30); do
    curl -sf --cacert "$dir/ca.pem" -o /dev/null https://127.0.0.1:8443/releases/latest/download/gui-checksums.txt && break
    sleep 1
  done
  curl -sSf --cacert "$dir/ca.pem" https://127.0.0.1:8443/releases/latest/download/gui-checksums.txt.minisig
  ;;
*)
  echo "unknown command $cmd" >&2
  exit 2
  ;;
esac
