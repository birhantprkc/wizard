# Wizard TUI demo — record with:
#   nix shell nixpkgs#vhs nixpkgs#ttyd nixpkgs#ffmpeg -c vhs demo/demo.tape
# (run from the repo root, with a release binary at target/release/wizard)
#
# The recording uses whatever provider ~/.wizard/config.toml activates —
# auto-approve is the default, so the agent edits without a modal. Make sure
# the active provider answers before recording (cloud account signed in, or a
# local server running).

Output demo/demo.gif
Output demo/demo.mp4

Set Shell "bash"
Set FontSize 15
Set Width 1200
Set Height 680
Set Padding 14
Set Framerate 20
Set TypingSpeed 40ms
# Palette mirrors the site's warm monochrome (bg #0b0b0a, cream #e9e6df,
# dim #94908a, faint #55524b). Ratatui maps White -> brightWhite, Gray ->
# white, DarkGray -> brightBlack, so those slots carry the bright/dim/faint
# text tiers.
Set Theme { "name": "wizard-warm", "background": "#0b0b0a", "foreground": "#e9e6df", "cursor": "#e9e6df", "black": "#11110f", "red": "#e06a55", "green": "#a3b285", "yellow": "#d6b36a", "blue": "#95a3b3", "magenta": "#b59a8e", "cyan": "#9cb3a8", "white": "#94908a", "brightBlack": "#55524b", "brightRed": "#f0836b", "brightGreen": "#b8c79a", "brightYellow": "#e8c98a", "brightBlue": "#aebccb", "brightMagenta": "#c9ada0", "brightCyan": "#b0c7bc", "brightWhite": "#f1eee6" }

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
# close the provider picker before composing (Esc cancels, leaves provider unchanged)
Escape
Sleep 800ms

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
