# Desktop app

`wizard app` opens the browser GUI in a native window.

```bash
wizard app            # open the window
wizard app --install  # add Wizard to the launcher / dock
```

It is the same thing `wizard gui` serves — same server, same agent core, same
sessions — with no browser tab and no URL to remember. One process: the GUI
server runs in-process, bound to a loopback port the OS picks, and the window
points at it. There is no sidecar, no IPC bridge, and nothing listening on a
fixed port that a running `wizard gui` could collide with.

Installing it:

```bash
curl -fsSL https://raw.githubusercontent.com/teddytennant/wizard/main/install.sh | WIZARD_APP=1 bash
```

That installs a second binary, `wizard-desktop`, beside `wizard`, and puts
Wizard in your launcher. `wizard` itself is unchanged (see
[Why a second binary](#why-a-second-binary)).

---

## Why a system webview, not Electron

Electron ships a copy of Chromium: ~200MB on disk, a second browser engine
resident in RAM, and a Node runtime whose security updates are now yours to
track. The whole frontend here is a few hundred kilobytes of framework-free ES
modules. Bundling a browser to render it would make the download two orders of
magnitude larger than the thing being downloaded.

So Wizard uses the webview the operating system already has, through
[`wry`] and [`tao`] — the two libraries Tauri is built on, used directly, no
Tauri. The shell adds roughly 10MB to the binary and starts with no Chromium
and no Node anywhere in the process.

[`wry`]: https://github.com/tauri-apps/wry
[`tao`]: https://github.com/tauri-apps/tao

**The caveat, stated plainly:** "the system webview" is not one engine. On
macOS it is WKWebView (Safari's engine). On Linux it is WebKitGTK. They are
both WebKit, but they are different builds of different vintages with different
bugs, and neither is the Chromium the GUI is most often developed against. The
frontend is deliberately vanilla — no framework, no build step, no bleeding-edge
CSS — which is what makes this affordable, but a rendering difference between
Linux and macOS is a *possible* bug class here in a way it is not for Electron.
If something renders wrong in the app and right in Chrome, that is the reason;
`wizard gui` in your own browser is always the fallback.

---

## Linux: WebKitGTK is required

The desktop build links WebKitGTK **at load time**. On a machine without it the
binary does not start at all — not the window, not `--version` — because the
dynamic loader fails before `main` runs. There is no way to catch that and print
something friendly, which is exactly why the shell is a separate binary and an
opt-in build.

| Distro | Package |
| --- | --- |
| Debian / Ubuntu | `libwebkit2gtk-4.1-0` (`-dev` to build) |
| Fedora | `webkit2gtk4.1` |
| Arch | `webkit2gtk-4.1` |
| NixOS | comes from the flake — `nix run github:teddytennant/wizard -- app` |

`install.sh` checks for the library before installing the app and tells you what
to install if it is missing, rather than leaving you with a binary that cannot
run.

macOS needs nothing: WKWebView is part of the OS.

### Wayland: the app runs under XWayland on purpose

On Wayland, WebKitGTK reports a **negative device pixel ratio** —
`devicePixelRatio` of `-0.0208`, `innerWidth` of `-138240` — so `100vh` resolves
to hundreds of millions of pixels and the layout explodes. It is an upstream
GTK 3 / WebKitGTK problem, not the GUI's CSS: a fourteen-line stock `wry`
example reproduces the identical number on the same machine (Hyprland, an output
at fractional scale 1.33), and GTK 3 has no fractional-scale support to fix it
with.

So on a Wayland session `wizard app` sets `GDK_BACKEND=x11` for itself before
GTK starts. XWayland ships with every mainstream Wayland desktop and renders the
window correctly (`devicePixelRatio` of 1); the cost is a slightly softer image
on a HiDPI screen, which beats a window you cannot use. An explicit setting wins,
so if your compositor renders GTK 3 correctly you can opt back in:

```bash
GDK_BACKEND=wayland wizard-desktop app
```

---

## `--install`: what it writes

Idempotent both ways — installing twice is fine, uninstalling something that was
never installed is fine.

### Linux

```
~/.local/share/applications/wizard.desktop
~/.local/share/icons/hicolor/scalable/apps/wizard.svg
~/.local/share/icons/hicolor/512x512/apps/wizard.png
```

The `.desktop` entry has `Terminal=false`, `Exec=<absolute path of the running
binary> app`, and a `StartupWMClass` that matches the window's GTK app id, so
the running window groups under the launcher entry instead of a generic one. It
passes `desktop-file-validate` clean. `update-desktop-database` and
`gtk-update-icon-cache` are run if present, and ignored if not.

Nothing is written outside `$HOME`. No root, no package manager.

### macOS

```
~/Applications/Wizard.app/Contents/Info.plist
~/Applications/Wizard.app/Contents/MacOS/Wizard      (execs the real binary)
~/Applications/Wizard.app/Contents/Resources/wizard.icns
```

`Contents/MacOS/Wizard` is a three-line shell trampoline, not a copy of the
binary: the bundle stays in step with whatever `wizard-desktop` is on disk, and
`wizard update` keeps the app current for free.

**No signing, no notarization, no Apple Developer account.** Gatekeeper gates
*quarantined* code — the `com.apple.quarantine` attribute that a browser or a
mail client sets on a file it downloaded. A bundle generated on your own machine
by `wizard app --install` never carries it, so it launches like anything you
built yourself. This is the reason the app is assembled locally instead of being
shipped as a `.dmg`.

The `.icns` is generated at install time from the embedded PNG with `sips` and
`iconutil` (both ship with macOS). If they are missing, the app installs without
an icon rather than failing.

### Uninstall

```bash
wizard app --uninstall
```

Removes exactly those files (on macOS, the whole `Wizard.app`). The binaries
stay; remove them with `rm`.

---

## Why a second binary

The desktop build is published as `wizard-desktop-<target>.tar.gz` and installed
as `wizard-desktop`. It is not a drop-in replacement for `wizard`, and
`WIZARD_APP=1` does not overwrite it.

The reason is the load-time link above. If the *default* binary linked WebKitGTK,
then every user on a machine without it — every server, every container, every
CI runner, everyone who only ever wanted the TUI — would have a `wizard` that
cannot start. The plain binary also has to be linkable statically against musl
(that is how it runs on NixOS and inside minimal images), and a static binary
cannot use a dynamically loaded system webview. The two requirements are
mutually exclusive, so they are two binaries.

Concretely, in `Cargo.toml`:

```toml
[features]
default = []
desktop = ["dep:tao", "dep:wry"]
```

A default `cargo build` compiles neither `tao` nor `wry` and links no webview.
That property is worth protecting: `ldd target/release/wizard` on the default
build should show libc, libm, libgcc and nothing else.

A `wizard-desktop` binary updates itself to desktop assets, so `wizard update`
from the app never quietly turns it back into the plain build.

---

## Window state

Size, position and maximized state persist in `~/.wizard/desktop.toml`. Delete
the file to get the default 1440×900 window back.

Links that leave the app — docs, a GitHub issue, an OAuth consent page — open in
your real browser, with your sessions and your extensions, rather than in a
window with no address bar and no way back.

`--devtools` opens the webview inspector:

```bash
wizard-desktop app --devtools
```

---

## Building it yourself

```bash
# Debian/Ubuntu: sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev
cargo build --release --features desktop
./target/release/wizard app --install
```

On NixOS, `nix develop` provides WebKitGTK and pkg-config, and the shell builds
from there.

The webview is not covered by the test suite: it needs a display server and a
system WebKit, which CI has neither of. What *is* tested is everything the
installer writes — the `.desktop` file, the `Info.plist`, the bundle layout, and
the idempotence of install and uninstall.
