#!/bin/sh
# Generate the release signing keypair and put its public half everywhere the
# tree expects to find it. Run once, ever, by the person who will hold the key.
#
#   contrib/seed-release-key.sh [keyfile]
#
# What it does, in order:
#   1. `minisign -G` into `<keyfile>` (default ~/.wizard-release.key), refusing
#      to touch an existing one.
#   2. Writes the public half to `wizard-release.pub`.
#   3. Rewrites the `WIZARD_RELEASE_PUBKEY=` line in `install.sh`.
#   4. Re-runs the two tests that pin those copies to each other.
#
# It does NOT commit, does NOT push, and does NOT upload the secret anywhere.
# The last step is yours and is printed at the end.
#
# ---------------------------------------------------------------------------
# Why this script exists rather than four commands in a runbook
#
# The public key lives in two places — `wizard-release.pub` and an inlined copy
# in `install.sh`, because a shell script cannot `include_str!` — and a test
# asserts they are identical. Updating one and not the other is a red suite;
# updating them in separate commits is a commit where `install.sh` would trust
# a key the binary does not. Both are easy to do by hand at 1am before a
# release and neither is fun to debug there.
#
# The other reason: `minisign -G` prompts for a password by default, and a
# passworded key cannot be used by CI, which signs unattended. The workflow
# feeds the secret in on stdin with `</dev/null`, so a passworded key hangs and
# then fails. This script passes `-W` deliberately and says so, rather than
# leaving somebody to discover it from a stuck job.
set -eu

cd "$(dirname "$0")/.."

KEYFILE="${1:-$HOME/.wizard-release.key}"
PUBFILE="wizard-release.pub"
PLACEHOLDER="RELEASE-SIGNING-KEY-NOT-YET-GENERATED"

die() { printf 'error: %s\n' "$1" >&2; exit 1; }

command -v minisign >/dev/null 2>&1 \
    || die "minisign is not on PATH. nix: 'nix shell nixpkgs#minisign'; brew: 'brew install minisign'; apt: 'apt install minisign'"

# Refuse to overwrite a key that already exists. Regenerating is not an
# ordinary operation: every release signed with the old key stops verifying
# against the new one, and `wizard update` treats that as tampering — which is
# exactly what it should do and exactly what you do not want to cause by
# re-running a script.
[ -e "$KEYFILE" ] \
    && die "$KEYFILE already exists. Regenerating a release key invalidates every signature made with the old one; move it aside deliberately if that is really what you want."

# And refuse to run over a tree that already carries a real key, for the same
# reason from the other direction.
if [ -f "$PUBFILE" ]; then
    case "$(grep -v '^untrusted comment:' "$PUBFILE" | tr -d '[:space:]')" in
        "${PLACEHOLDER}"*) ;;
        "") ;;
        *) die "$PUBFILE already holds a real key. This script seeds the first one; it does not rotate." ;;
    esac
fi

printf '==> Generating the release keypair\n'
# -W: no password. CI signs unattended and feeds the secret key in on stdin, so
# a passworded key would hang the release job rather than fail it clearly.
#
# -f because `wizard-release.pub` is committed to the repository holding the
# placeholder, and `minisign -G` refuses to write over an existing public key
# file. The two guards above have already established what -f is allowed to
# destroy here: the secret key does not exist, and the public file is the
# placeholder or empty. Without this the script fails on its first run for
# everybody, which is how it failed on its first run for me.
minisign -G -W -f -p "$PUBFILE" -s "$KEYFILE" >/dev/null

KEY_LINE="$(grep -v '^untrusted comment:' "$PUBFILE" | tr -d '[:space:]')"
[ -n "$KEY_LINE" ] || die "minisign wrote no key line to $PUBFILE"
case "$KEY_LINE" in
    "${PLACEHOLDER}"*) die "refusing to continue: $PUBFILE still reads as the placeholder" ;;
esac

printf '==> Inlining the public key into install.sh\n'
# The whole line is replaced rather than the value edited in place: the value
# is base64 and can contain `/` and `+`, which makes a naive sed expression a
# quoting puzzle. Writing the line out avoids the question.
awk -v key="$KEY_LINE" '
    /^WIZARD_RELEASE_PUBKEY=/ { print "WIZARD_RELEASE_PUBKEY=\"" key "\""; next }
    { print }
' install.sh > install.sh.seeded
grep -q "^WIZARD_RELEASE_PUBKEY=\"${KEY_LINE}\"$" install.sh.seeded \
    || { rm -f install.sh.seeded; die "could not rewrite WIZARD_RELEASE_PUBKEY in install.sh"; }
# Preserve the mode: install.sh is executable and `curl | bash` users are not
# the only consumers — CI runs it directly.
chmod --reference=install.sh install.sh.seeded 2>/dev/null || chmod 755 install.sh.seeded
mv install.sh.seeded install.sh

printf '==> Verifying the two copies agree\n'
cargo test --locked the_installer_and_the_binary_trust_the_same_key >/dev/null 2>&1 \
    || die "the pinning test still fails; install.sh and $PUBFILE disagree"
cargo test --locked a_public_key_that_is_not_one_refuses_the_update >/dev/null 2>&1 \
    || die "the key this wrote is not one the verifier accepts"

cat <<EOF

Done. The secret key is at:

    $KEYFILE

It is NOT in the repository and must never be. Back it up somewhere you would
trust with a signing identity — losing it means every future release has to be
signed by a new key, and every installed copy of Wizard will refuse the first
release signed with it.

Two things left, both yours:

  1. Give CI the secret key. The whole file, both lines, including the
     'untrusted comment:' line:

         gh secret set MINISIGN_SECRET_KEY < "$KEYFILE"

  2. Commit the public half. Both files in ONE commit — a commit with only one
     of them is a tree where install.sh and the binary trust different keys,
     and the test above is what will tell you so:

         git add $PUBFILE install.sh
         git commit -m 'seed the release signing key'

After that, \`install.sh\` stops refusing every download, and the four distro
legs in CI start exercising the verification path instead of stopping at the
placeholder.
EOF
