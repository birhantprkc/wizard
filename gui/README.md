# Wizard GUI

The desktop app for Wizard. It runs Wizard, Pi and Claude Code side by side across your projects, and on machines you reach over SSH.

Wizard GUI is a fork of [Zeron](https://github.com/zeronsh/zeron), the open-source agent controller built by the Zeron team ([zeron.sh](https://zeron.sh)). Nearly all of it is their work: the engine, sync, transcript, composer and gpui interface. Thank you to Zeron's creators. It stays under their MIT license (see [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)).

`wizard gui` opens this app once it is installed.

What the fork changes:

- Only Wizard, Pi and Claude Code are offered, and Wizard is the default.
- First-run onboarding installs the agents and lets Wizard reuse a Codex (ChatGPT) or Grok CLI sign-in it finds on disk.
- SSH devices: Settings, Devices, Add SSH device. It uses your SSH keys, installs the engine and Wizard on the remote, can share this machine's Wizard sign-in, and starts the engine there. The machine then shows up in the device menu above the message box.
- Plugins: Settings, Plugins browses the Pi package gallery and installs a package for Pi, for Wizard, or both. Wizard support is in beta: skills and prompts work, extensions don't yet. Before installing, the page shows which parts of the package work in Wizard. It runs `wizard plugins` on the device, so remote devices work too (see [docs/pi-plugins.md](../docs/pi-plugins.md)).
- No update check against Zeron's release feed, since that would replace this build with upstream Zeron.

## Keyboard

Settings, Shortcuts has two switches, both off by default and independent of each other.

**Power user mode** drives the whole app from the keyboard with vim-style keys. Esc leaves the message box (or a settings page) for navigation, a NAV tag in the corner says which pane has the cursor, and a ring marks the pane and the row. `i` goes back to typing. Keys never fire inside a text field. `?` lists every key, and after a prefix like Space or `g` a hint shows what can follow. The slash menu also gains the app's own commands (`/agent`, `/effort`, `/device`, `/archive`, `/export`, `/theme`, `/keys`, `/sidebar`, `/panel`, `/next`, `/prev`, `/vim`), argument hints, and fuzzy matching, next to whatever the agent advertises.

| Keys | Does |
| --- | --- |
| `j` `k`, `gg` `G`, Enter | Move through sessions or messages, open |
| `h` `l`, `Ctrl-w h/j/k/l/w` | Move between sidebar, transcript, message box, right panel |
| `o`, `y`, `Ctrl-d` `Ctrl-u` | Expand a tool card, copy a message or code block, half page |
| `x` or `dd`, `D` | Archive or delete a session (asks first) |
| `gt` `gT`, `1`-`9` | Right panel tabs |
| `c` or `Space n`, `Space a/m/e/d` | New chat, switch agent, model, effort, device |
| `Space ,`, `:`, `/`, `?` | Settings, command palette, search sessions, all keys |

**Vim editing in the message box** gives the composer normal, insert, visual and visual-line modes: motions, counts, operators with text objects (`ciw`, `da(`, `yi"`), `p`, `u`, `Ctrl-r`, `.` and `>>`. A tag above the box shows the mode and any pending keys. In insert mode Enter works as before; in normal mode Enter sends. With both switches on, Esc in normal mode leaves the box for navigation.

Screenshots are in [docs/screenshots/power-user](docs/screenshots/power-user).

## Install

Every Wizard release from 3.5.0 on carries the app:

- Linux: `wizard-gui-<version>-linux-<arch>.tar.gz`. Unpack it and run `./install.sh`, which puts `zeron` in `~/.local/bin` and adds a desktop entry.
- macOS (Apple silicon): `wizard-gui-<version>-macos-arm64.dmg`. It is not signed or notarized unless the release had signing credentials, so the first launch needs right click, Open.
- Windows: `wizard-gui-<version>-windows-x86_64.zip`. Unpack it and run `zeron.exe`.

Updates come from the same releases page; the app does not update itself.

## Build from source

```bash
cd gui
cargo run --release -p zeron
```

Linux needs the gpui libraries: xkbcommon, wayland, x11/xcb, fontconfig, freetype, alsa, vulkan and webkit2gtk-4.1 (the list the release workflow installs is in `.github/workflows/release.yml`). The sidebar browser also needs the [Linux browser runtime](docs/reference/linux-browser.md). Windows source builds are covered in [docs/reference/windows-development.md](docs/reference/windows-development.md).

Day to day:

```bash
zeron status
zeron daemon start|stop|restart|status
```

Everything is local by default. The optional multi-device sync (`zeron login`) goes through Zeron's hosted service.

How the pieces fit is in [ARCHITECTURE.md](ARCHITECTURE.md).
