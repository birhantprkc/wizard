# Wizard GUI

The desktop app for Wizard. It runs Wizard, Pi and Claude Code side by side across your projects, and on machines you reach over SSH.

Wizard GUI is a fork of [Zeron](https://github.com/zeronsh/zeron), the open-source agent controller built by the Zeron team ([zeron.sh](https://zeron.sh)). Nearly all of it is their work: the engine, sync, transcript, composer and gpui interface. Thank you to Zeron's creators. It stays under their MIT license (see [LICENSE](LICENSE) and [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)).

`wizard gui` opens this app once it is installed.

What the fork changes:

- Only Wizard, Pi and Claude Code are offered, and Wizard is the default.
- First-run onboarding installs the agents and lets Wizard reuse a Codex (ChatGPT) or Grok CLI sign-in it finds on disk.
- SSH devices: Settings, Devices, Add SSH device. It uses your SSH keys, installs the engine and Wizard on the remote, can share this machine's Wizard sign-in, and starts the engine there. The machine then shows up in the device menu above the message box.
- Pi extensions: Settings, Pi extensions browses the Pi package gallery and installs packages in one click.
- No update check against Zeron's release feed, since that would replace this build with upstream Zeron.

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
