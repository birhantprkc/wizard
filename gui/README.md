# Wizard GUI

Wizard GUI is the desktop app for [Wizard](https://github.com/teddytennant/wizard). It runs Wizard, Pi, and Claude Code side by side across your projects, and on the machines you reach over SSH.

**Credit:** Wizard GUI is a fork of [Zeron](https://github.com/zeronsh/zeron), the open-source agent controller built by the Zeron team ([zeron.sh](https://zeron.sh)). Nearly everything here is their work: the engine, sync, transcript, composer, and gpui interface. Thank you to Zeron's creators. It is released under the same MIT license.

What this fork changes:

- **Agents:** only Wizard, Pi, and Claude Code are offered, and Wizard is the default.
- **First-run onboarding:** installs the agents, and lets Wizard reuse a Codex (ChatGPT) or Grok CLI sign-in it finds on disk.
- **SSH devices:** Settings → Devices → Add SSH device. It uses your own SSH keys, installs the engine and Wizard on the remote, can share this device's Wizard sign-in, and starts the engine there. The machine then appears in the device menu above the message box, and its chats, terminals, files, and changes work from the same window over an SSH tunnel.
- **Pi extensions:** Settings → Pi extensions browses the Pi package gallery and installs packages in one click.
- **Defaults:** compact transcripts are on, the Starship artwork sits behind new Wizard chats, the sidebar has a Settings gear, and chat rows carry no project badges.

The upstream Zeron README follows.

---

## Zeron

Control your coding agents (Claude Code, Codex, Cursor, Devin, Grok, Hermes, Pi, Antigravity) locally by default, with optional multi-device sync.

*English | [简体中文](README.zh-CN.md)*

![Zeron driving a Claude Code session with a live branch diff sidebar](apps/landing/public/assets/app-screenshot.jpg)

Every device runs a small engine that stores sessions on that device. A new installation starts in local-only mode without an account or a network connection.

## Install and run locally (Linux)

```bash
curl -fsSL https://zeron.sh/install.sh | sh
zeron status
```

The installer starts the daemon immediately and keeps it running across reboots. No sign-in or sync configuration is required.

The desktop sidebar browser also needs the [Linux browser runtime](docs/reference/linux-browser.md).

Day-to-day:

```bash
zeron status      # local/synced mode and engine status
zeron update      # update to the latest release
zeron daemon start|stop|restart|status
```

## Optional multi-device sync

Sign in only when you want to open your account's synced workspace. Authentication changes the profile selected by the next engine start, so stop the daemon before changing it:

```bash
zeron daemon stop
zeron login
zeron daemon start
```

You can then start an agent on one synced device and follow or drive it from another. An always-on machine such as a VPS can keep those agents working after you close your laptop.

Devices signed in to the same synced account are trusted with remote workspace access. A device controlling a workspace on another device can list, read, and write its files; enabling `Show ignored files` also makes gitignored files such as `.env` available remotely. `.git` is always excluded. Only sign in devices you trust with the full contents of your workspaces.

Signing in does not upload, move, or import existing local sessions. Local sessions and their attachments remain under the local profile and reappear when you return to local-only mode:

```bash
zeron daemon stop
zeron logout
zeron daemon start
```

`zeron login` and `zeron logout` refuse to modify credentials while an engine owns the data directory. The desktop app follows the same next-restart profile boundary.

On macOS: use the desktop release, or build `zeron` from source and run `zeron daemon install` to install the launchd service.

On Windows: extract the portable release ZIP and run `zeron.exe`. Keep `zeron-update.json` beside it for in-app updates. See the [development notes](docs/reference/windows-development.md) for source builds.

## Sponsors

Thank you to [The Context Company](https://www.thecontextcompany.com/) for sponsoring Zeron.

You can help fund Zeron's development too. Individuals and companies are welcome to [become a sponsor on GitHub](https://github.com/sponsors/zeronsh).

---

Developing or curious how it works? [![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/zeronsh/zeron) or check out [ARCHITECTURE.md](ARCHITECTURE.md).

Licensed under the [MIT License](LICENSE).
