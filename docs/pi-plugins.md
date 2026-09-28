# Pi plugins

Wizard installs [Pi](https://pi.dev) packages. This is in beta: skills and prompt
templates work, extensions and themes don't yet.

A Pi package bundles skills, prompt templates, TypeScript extensions and themes.
The gallery at [pi.dev/packages](https://pi.dev/packages) lists every npm package
tagged `pi-package`.

## From the terminal

```sh
wizard plugins search pdf
wizard plugins inspect npm:@scope/pi-tools     # what would work, installs nothing
wizard plugins install npm:@scope/pi-tools     # for Wizard
wizard plugins install pi-tools --for both     # for Wizard, and `pi install` too
wizard plugins list
wizard plugins remove pi-tools [--for wizard|pi|both]
```

A spec is anything `pi install` takes: an npm name, `npm:<name>@<version>`,
`git:github.com/<owner>/<repo>@<ref>`, a git URL, or a local directory. `--for`
defaults to `wizard`. `--for pi` runs `pi install` or `pi remove`. Every verb takes
`--json`, which prints one object with a `schema` version. Wizard GUI reads that.

In the TUI, `/plugins` searches the gallery in a picker. Enter asks whether the
package goes to Pi and Wizard, only Wizard, or only Pi, and just installs it for
Wizard when Pi isn't installed. `/plugins list` shows what is installed and Enter
removes one. `/plugins install <spec> [--for ...]` and `/plugins remove <name>`
skip the pickers. When an install finishes, skills and commands reload, so you
don't need `/reload`.

Wizard GUI does the same from Settings, Plugins, for whichever device the page
targets.

## What maps

| In the package | In Wizard |
| --- | --- |
| Skills (`SKILL.md` directories, or a loose `.md` in a skills root) | Copied whole to `~/.wizard/skills/<name>/` |
| Prompt templates (`.md`) | Commands in `~/.wizard/commands/<name>.md` |
| Extensions (`.ts`, `.js`) | Not supported yet |
| Themes (`.json`) | Not supported yet |

Discovery follows Pi's: the `pi` key in `package.json` when there is one (paths,
globs, and `!`, `+`, `-` overrides), otherwise the `skills/`, `prompts/`,
`extensions/` and `themes/` directories.

Skills use the same Agent Skills format in both, so they work as they are. Some
frontmatter keys don't carry over, and `inspect` and `install` list them per skill:

- `disable-model-invocation: true` is Pi-only. Wizard still lists the skill to the
  model, and has no `/skill:name` command.
- A skill with no `description` is skipped by Pi. Wizard lists it by name only.

Prompt templates become commands with `syntax: pi` in their frontmatter, so they
expand the way Pi expands them: `$1`, `$@`, `$ARGUMENTS`, `${1:-default}`,
`${@:2}`, `${@:2:1}`, with quoted arguments kept together. The description comes
from the frontmatter, or from the first line, as in Pi. A template named after a
built-in command (`/model`, `/plan`) is skipped, since the built-in would always
win.

## Bookkeeping

What an install wrote is recorded in `~/.wizard/pi-plugins.json`. `remove` deletes
exactly those paths, and a reinstall drops what the new version no longer has. An
install never overwrites a skill or command you wrote, or one another package
installed; that item is skipped and the output says why. A package with nothing
Wizard can run is refused for Wizard.

Wizard doesn't run `npm install` for a package's dependencies. A skill script that
imports one needs `npm install` in its own directory, and the output notes when a
package declares dependencies.

## Trust

A package's skills steer the agent, and its scripts run with your permissions.
Only install packages you trust. For Wizard, an npm download is checked against the
registry's sha512 before it is unpacked, and nothing from the package runs during the
install. `--for pi` hands the package to `pi install`, which runs npm.
