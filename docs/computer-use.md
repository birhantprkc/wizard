# Computer use (desktop control)

Wizard can drive your desktop — move and click the mouse, type, press key
chords, scroll, and take screenshots — through the native `computer` tool. A
vision-capable model uses it to operate GUI applications the same way a person
would: look at the screen, decide, act, look again.

This is real control of your machine, and like every Wizard tool it runs with
your privileges. **There is no per-action approval gate**: a click, a keystroke
and a drag happen the moment the model asks for them, with nothing between the
request and your desktop. `computer` is an `Execute`-access tool, which means
plan mode refuses it and a read-only subagent never gets it — that is the whole
of what the access class does. Read [SECURITY.md](../SECURITY.md) and prefer a
VM or a throwaway session for autonomous runs.

> **Vision required.** Screenshots are only useful to a model that can see
> images: Claude, GPT-4o-class, Grok vision, or a local vision model (e.g. a
> Qwen-VL / Llama-Vision GGUF). A text-only model can still move the mouse and
> type from coordinates you give it, but it cannot read the screen.

## One-time setup

Run the bundled setup command, then follow its final instructions:

```bash
wizard desktop-setup
```

### Linux

`desktop-setup` detects your distribution and installs the pieces Wizard shells
out to:

- **`ydotool`** (+ the `ydotoold` daemon) — input synthesis via the kernel
  `uinput` interface, so it works on **Wayland and X11** alike.
- **`grim`** (Wayland) and **`maim`** (X11) — screen capture. Both are
  installed, because which one is the working tool is a property of the session
  you happen to be logged into, not of the machine. On X11, ImageMagick's
  `import` is used as a fallback if it is already present; it is not installed
  for you.
- **`slurp`** — the region picker `grim` pairs with. Wizard never invokes it
  itself (it captures whole screens); it is installed so the same setup covers
  taking a region by hand.
- **`at-spi2-core`**, **`xdg-desktop-portal(-gtk)`** — the accessibility and
  portal stack used by modern desktops.

It also installs a `udev` rule so the `input` group can open `/dev/uinput`, adds
you to that group, and enables the `ydotoold` user service.

**You must log out and back in** (or reboot) after setup — group membership
does not apply to existing sessions, and until it does `/dev/uinput` access is
denied.

Supported package managers: `apt` (Debian/Ubuntu/Pop!_OS/Mint), `dnf`
(Fedora/RHEL/Rocky), `pacman` (Arch/CachyOS/EndeavourOS), `zypper` (openSUSE).
Other distros: install the packages above by hand, then re-run `desktop-setup`
to finish the udev/group/service steps.

#### NixOS

NixOS is declarative, so `desktop-setup` prints the config to add instead of
installing imperatively:

```nix
programs.ydotool.enable = true;            # ydotool + ydotoold + uinput rule
environment.systemPackages = with pkgs; [
  grim slurp maim ydotool                  # capture (Wayland + X11) + input
  at-spi2-core xdg-desktop-portal xdg-desktop-portal-gtk
];
users.users.<you>.extraGroups = [ "input" "uinput" ];
```

Then `sudo nixos-rebuild switch` and re-log. To try it without a rebuild:
`nix profile install nixpkgs#ydotool nixpkgs#grim nixpkgs#slurp nixpkgs#maim`
and `systemctl --user start ydotoold`. `desktop-setup` closes by naming which of
the four binaries are not on `PATH` yet.

### macOS

Nothing to install — input goes through the built-in **CoreGraphics**
(`CGEvent`) automation API and capture through `screencapture`. You only have to
grant two permissions to the terminal (or app bundle) you launch Wizard from,
under **System Settings → Privacy & Security**:

1. **Accessibility** — enable your terminal (Terminal, iTerm, Ghostty, VS Code,
   …). Without it, mouse and keyboard events are silently dropped.
2. **Screen Recording** — enable the same terminal. Without it, screenshots come
   back blank.

Fully quit and reopen the terminal after granting each one. `wizard
desktop-setup` prints these steps on macOS.

## Verifying

```bash
wizard -p "take a screenshot of my desktop and describe what you see"
```

A vision model should return a description. If input fails, re-check the log
out/in step (Linux) or the Accessibility grant (macOS).

## How the model drives it

The tool exposes a single `computer` function with an `action` argument:

| action            | arguments                          | effect                                   |
| ----------------- | ---------------------------------- | ---------------------------------------- |
| `screenshot`      | —                                  | capture the screen, return the image     |
| `mouse_move`      | `x`, `y`                           | move the pointer                         |
| `left_click`      | `x`, `y` (optional)                | click (at a point, or where it is)       |
| `right_click`     | `x`, `y` (optional)                | right click                              |
| `middle_click`    | `x`, `y` (optional)                | middle click                             |
| `double_click`    | `x`, `y` (optional)                | double click                             |
| `left_click_drag` | `x`, `y`                           | press, drag to `(x, y)`, release         |
| `type`            | `text`                             | type a string                            |
| `key`             | `text` (e.g. `ctrl+c`, `Return`)   | press a key chord                        |
| `scroll`          | `scroll_direction`, `scroll_amount`| scroll up/down/left/right                |
| `cursor_position` | —                                  | report the pointer position (if known)   |
| `wait`            | `duration` (seconds)               | pause (e.g. for a UI to settle)          |

`x`/`y` may also be given as a `coordinate: [x, y]` array.

### Coordinates

Coordinates are **real screen pixels**, origin top-left. Every `screenshot`
reports the true screen size, and the model works in that space — so its clicks
map back to the screen 1:1. The returned image may be downscaled for transport
(to keep large or multi-monitor captures from bloating the request); the model
reasons about positions proportionally against the reported size.

### Key chords

`key` accepts `+`-separated chords: modifiers `ctrl`, `shift`, `alt`/`option`,
`meta`/`super`/`cmd`, plus a key name (`Return`, `Tab`, `Escape`, `Up`, `Home`,
`PageDown`, `F5`, a letter, a digit, …). Examples: `ctrl+c`, `cmd+shift+t`,
`alt+Tab`.

## Limitations

- **Scroll on Linux** is approximated with arrow keys — `ydotool` 1.0 has no
  wheel command. It works for most scrollable views; macOS uses real wheel
  events.
- **`cursor_position` on Linux** is only available where the compositor can
  report it (e.g. Hyprland via `hyprctl`); elsewhere it returns "unsupported".
- **Fractional / HiDPI scaling on Wayland** can offset coordinates on some
  compositors; capture and click are most reliable at scale 1. macOS reports
  logical points and is Retina-correct.
- Windows is not supported.
