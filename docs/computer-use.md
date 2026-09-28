# Computer use

Wizard can drive a desktop through the native `computer` tool: take
screenshots, move and click the mouse, type, press key chords and scroll. A
vision-capable model uses it to operate GUI programs the way a person would:
look, act, look again.

Computer use is off until you set it up. With it off, the model does not get
the tool at all; if a task needs a screen, it tells you and offers to set it
up.

**There is no per-action approval gate.** A click or a keystroke happens the
moment the model asks for it. `computer` is an `Execute`-access tool, which
means plan mode refuses it and read-only subagents never get it, and that is
all the access class does. Read [SECURITY.md](../SECURITY.md). The VM backend
exists so an autonomous run does not have to touch your own desktop.

> **Vision required.** Screenshots are only useful to a model that can see
> images (Claude, GPT-4o class, Grok, or a local vision model).

## Setting it up

```bash
wizard computer setup
```

It reports what it found (OS, Wayland or X11, compositor, which tools are
installed, whether Docker or Podman is available), asks whether to use a VM or
this desktop, checks the choice by taking a screenshot and moving the pointer
one pixel and back, and only then writes this to `~/.wizard/config.toml`:

```toml
[computer]
enabled = true
backend = "vm"        # or "host"
driver = "native"     # host only: "native" or "scripted"
```

In a running session, `/reload` picks it up. `/computer` shows the same report
as `wizard computer status`. You can also just ask the agent to set it up; the
`computer` page of its manual tells it how.

Other commands:

| command | what it does |
| --- | --- |
| `wizard computer status` | the report, changes nothing |
| `wizard computer setup --backend vm --yes` | set up without questions |
| `wizard computer check [--backend host\|vm] [--save shot.png]` | screenshot plus a one-pixel pointer nudge |
| `wizard computer vm up [--rebuild]` / `down` / `status` | run the VM |
| `wizard computer disable` | turn it off again |

## Backends

### VM (recommended)

A desktop in a local container: Alpine with Xvfb, the fluxbox window manager,
xterm and x11vnc. `wizard computer vm up` builds the image the first time (the
recipe is `contrib/computer-vm/`, compiled into the binary), starts the
container, and publishes VNC on `127.0.0.1:5905` only. The `computer` tool
talks RFB (VNC) to it for both screenshots and input. Right-click the desktop
for its menu.

No GPU is needed. Settings live under `[computer.vm]`: `engine` (`docker` or
`podman`), `image`, `container`, `port`, `width`, `height`. To drive a VM you
run yourself, such as QEMU with `-vnc :1`, set `address = "127.0.0.1:5901"`;
Wizard then connects there and leaves starting and stopping to you. The VNC
server must not ask for a password, so keep it on localhost.

VNC was chosen over running `xdotool` inside the container because one
connection carries both frames and input with no process per action, the same
client serves Wizard GUI's live panel and its take-control mode, and any VM
that speaks VNC works unchanged.

### This desktop, built-in driver

| system | driver |
| --- | --- |
| Linux, X11 | ydotool input, maim capture |
| Linux, Wayland on Hyprland, sway, river, wayfire and other wlroots compositors | ydotool input, grim capture |
| macOS | CoreGraphics input, `screencapture` (not verified on real hardware yet) |

`wizard desktop-setup` installs the Linux tools (apt, dnf, pacman, zypper),
adds a uinput udev rule, puts you in the `input` group and enables
`ydotoold`. Log out and back in afterwards. On NixOS it prints the config to
add instead:

```nix
programs.ydotool.enable = true;
environment.systemPackages = with pkgs; [ grim slurp maim ydotool ];
users.users.<you>.extraGroups = [ "input" "uinput" ];
```

On macOS, grant Accessibility and Screen Recording to the terminal you run
Wizard from, under System Settings, Privacy & Security, then restart it.

### This desktop, generated driver

Some desktops have no built-in driver: GNOME and KDE on Wayland (grim needs
wlr-screencopy, which their compositors do not offer), other Wayland
compositors Wizard does not know, Windows, and WSL. There, `setup` shows a
plan (for example spectacle for capture on KDE, the GNOME Shell screenshot
D-Bus call on GNOME, PowerShell on Windows) and asks whether the agent should
write the tools. On yes it runs the agent, which writes
`~/.wizard/tools/computer_driver.toml` and `computer_driver.lua`, a normal
LuaJIT scripted tool, then checks it the same way as the others and sets
`driver = "scripted"`.

The driver is called once per action with an `args` table:

| `action` | other fields | must |
| --- | --- | --- |
| `screenshot` | `path` | write a PNG of the whole screen to `path` |
| `move` | `x`, `y` | move the pointer |
| `click` | `button` (`left`, `right`, `middle`), `count` | click where the pointer is |
| `drag` | `x`, `y` | press, move to `(x, y)`, release |
| `type` | `text` | type it |
| `key` | `chord` | press a chord like `ctrl+c` |
| `scroll` | `direction`, `amount` | scroll |
| `cursor` | | print `x,y`, or error |

A Lua error fails the action. You can read, edit or delete the files like any
other scripted tool. The Windows and WSL plans are written but not verified.

## Watching it in Wizard GUI

When computer use is active, Wizard GUI shows a small live screen card in the
top right of the window. Click it to open it large. For the VM it is live: the
GUI opens its own VNC connection to the address `vm up` recorded in
`~/.wizard/computer/vm.json`. For this desktop it shows the agent's latest
screenshot. Either way the agent's last action is drawn on top: a ripple where
it clicked, a caption for what it typed.

**Take control** pauses the agent and, for the VM, sends your mouse and
keyboard to that screen. The GUI writes `~/.wizard/computer/control`; while it
exists the `computer` tool holds every action except screenshots for up to 30
seconds, then tells the model you have the screen and to look again before
acting. **Give back** removes it. A lease left by a process that has exited is
ignored.

The terminal UI and sovereign mode show only the tool call lines, as before.

## How the model drives it

One `computer` function with an `action` argument:

| action | arguments | effect |
| --- | --- | --- |
| `screenshot` | | capture the screen, return the image |
| `mouse_move` | `x`, `y` | move the pointer |
| `left_click`, `right_click`, `middle_click`, `double_click` | `x`, `y` (optional) | click at a point, or where the pointer is |
| `left_click_drag` | `x`, `y` | press, drag to `(x, y)`, release |
| `type` | `text` | type a string |
| `key` | `text` (`ctrl+c`, `Return`) | press a chord |
| `scroll` | `scroll_direction`, `scroll_amount` | scroll |
| `cursor_position` | | report the pointer position, if known |
| `wait` | `duration` (seconds) | pause |

Coordinates are real screen pixels with the origin top left; `coordinate:
[x, y]` works too. Every screenshot reports the true screen size, and the
image may be downscaled for transport. Chords join modifiers (`ctrl`, `shift`,
`alt`, `super`/`cmd`) and a key (`Return`, `Tab`, `Escape`, `Up`, `PageDown`,
`F5`, a letter, a digit) with `+`.

## Limitations

- On the Linux host driver, scroll is arrow keys (ydotool 1.0 has no wheel).
  The VM gets real wheel events.
- `cursor_position` works on Hyprland and in the VM (last known position),
  and returns "unsupported" elsewhere on Linux.
- Fractional scaling on Wayland can offset coordinates on some compositors.
- The VM backend connects only to VNC servers without a password.
