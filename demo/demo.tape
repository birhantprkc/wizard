# Wizard TUI demo — record with:
#   nix shell nixpkgs#vhs nixpkgs#ttyd nixpkgs#ffmpeg -c vhs demo/demo.tape
# (run from the repo root, with a release binary at target/release/wizard)
#
# The recording uses whatever provider ~/.wizard/config.toml activates —
# auto-approve is the default, so the agent edits without a modal. Make sure
# the active provider answers before recording (cloud account signed in, or a
# local server running).

Output demo/demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1200
Set Height 680
Set Padding 14
Set Framerate 20
Set TypingSpeed 40ms
Set Theme { "name": "wizard-dark", "background": "#101014", "foreground": "#e8e6f0", "cursor": "#d36ee6", "black": "#16161c", "red": "#f25e74", "green": "#5fd6a0", "yellow": "#e6c36e", "blue": "#7aa5f0", "magenta": "#d36ee6", "cyan": "#6ed6e6", "white": "#d8d6e0", "brightBlack": "#4a4a56", "brightRed": "#ff7a8e", "brightGreen": "#7af0ba", "brightYellow": "#ffd98e", "brightBlue": "#9ec0ff", "brightMagenta": "#ea8ef5", "brightCyan": "#8ef0ff", "brightWhite": "#f4f2fa" }

# --- hidden setup: throwaway copy of the sample repo --------------------------
Hide
Type "mkdir -p /tmp/wizard-demo-bin && ln -sf ${WIZARD_DEMO_BIN:-$PWD/target/release/wizard} /tmp/wizard-demo-bin/wizard && export PATH=/tmp/wizard-demo-bin:$PATH"
Enter
Type "rm -rf /tmp/wizard-demo && cp -r ${WIZARD_DEMO_SRC:-$PWD/demo}/sample-project /tmp/wizard-demo && cd /tmp/wizard-demo && git init -q && git add -A && git -c user.name=Demo -c user.email=demo@example.com commit -qm initial && clear"
Enter
Sleep 1s
Show

# --- launch -------------------------------------------------------------------
Type "wizard"
Sleep 800ms
Enter
Sleep 4s
Screenshot demo/frame-1.png
Sleep 500ms

# --- providers at a glance ----------------------------------------------------
Type@60ms "/provider"
Sleep 800ms
Enter
Sleep 2.5s
Screenshot demo/frame-2.png
Sleep 1s

# --- ask for the fix ----------------------------------------------------------
Type@45ms 'Read main.py and fix the add() bug: it subtracts instead of adding.'
Sleep 1.2s
Enter

# spinner verb while the agent works (no approval modal — edits apply directly)
Sleep 3s
Screenshot demo/frame-3.png

# let the agent edit and finish its reply; the idle footer ("history") only
# returns once the turn is over
Wait+Screen@120s /history/
Sleep 2s
Screenshot demo/frame-4.png
Sleep 2s

# --- show the diff sidebar ----------------------------------------------------
Type@60ms "/diff"
Sleep 1s
Enter
Sleep 4s
Screenshot demo/frame-5.png
Sleep 4s

# --- quit ---------------------------------------------------------------------
Type@60ms "/quit"
Sleep 600ms
Enter
Sleep 2s
