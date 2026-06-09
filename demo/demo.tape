# Wizard TUI demo — record with:
#   nix shell nixpkgs#vhs nixpkgs#ttyd nixpkgs#ffmpeg -c vhs demo/demo.tape
# (run from the repo root)
#
# Model: qwen3:0.6b is the smallest model that fits this machine (14 GiB RAM).
# For a launch-quality recording on a GPU box, change the Env line below to a
# qwen3.6-tier model — nothing else in the tape needs to change.

Output demo/demo.gif

Set Shell "bash"
Set FontSize 15
Set Width 1200
Set Height 680
Set Padding 14
Set Framerate 20
Set TypingSpeed 40ms
Set Theme { "name": "wizard-dark", "background": "#101014", "foreground": "#e8e6f0", "cursor": "#d36ee6", "black": "#16161c", "red": "#f25e74", "green": "#5fd6a0", "yellow": "#e6c36e", "blue": "#7aa5f0", "magenta": "#d36ee6", "cyan": "#6ed6e6", "white": "#d8d6e0", "brightBlack": "#4a4a56", "brightRed": "#ff7a8e", "brightGreen": "#7af0ba", "brightYellow": "#ffd98e", "brightBlue": "#9ec0ff", "brightMagenta": "#ea8ef5", "brightCyan": "#8ef0ff", "brightWhite": "#f4f2fa" }

Env WIZARD_MODEL "qwen3:0.6b"

# --- hidden setup: throwaway copy of the sample repo --------------------------
Hide
Type "mkdir -p /tmp/wizard-demo-bin && ln -sf /home/gradient/projects/ai/wizard/target/release/wizard /tmp/wizard-demo-bin/wizard && export PATH=/tmp/wizard-demo-bin:$PATH"
Enter
Type "rm -rf /tmp/wizard-demo && cp -r /home/gradient/projects/ai/wizard/demo/sample-project /tmp/wizard-demo && cd /tmp/wizard-demo && git init -q && git add -A && git -c user.name=Demo -c user.email=demo@example.com commit -qm initial && clear"
Enter
Sleep 1s
Show

# --- launch -------------------------------------------------------------------
Type "wizard"
Sleep 800ms
Enter
Sleep 4.5s
Screenshot demo/frame-1.png
Sleep 1s

# --- ask for the fix ----------------------------------------------------------
Type@45ms 'In main.py, replace "a - b" with "a + b" to fix the add() bug.'
Sleep 1.2s
Enter

# wait for the approval modal (edit_file is gated in genie mode)
Wait+Screen@90s /y approve/
Screenshot demo/frame-2.png
Sleep 4s
Type "y"

# let the agent finish its reply (streaming)
Sleep 14s
Screenshot demo/frame-3.png
Sleep 2s

# --- show the diff sidebar ----------------------------------------------------
Type@60ms "/diff"
Sleep 800ms
Enter
Sleep 3.5s
Screenshot demo/frame-4.png
Sleep 4s

# --- quit ---------------------------------------------------------------------
Type@60ms "/quit"
Sleep 600ms
Enter
Sleep 2s
