//! The generated host driver: a LuaJIT scripted tool the agent writes for a
//! system the built-in driver does not cover (GNOME or KDE on Wayland,
//! Windows, WSL), and the `computer` tool calls through.
//!
//! It lives where every scripted tool lives, `~/.wizard/tools/`, as
//! `computer_driver.toml` plus `computer_driver.lua`, so it is an ordinary
//! tool after `/reload` and the user can read, edit or delete it. With
//! `[computer] driver = "scripted"`, [`ScriptedBackend`] runs it once per
//! action with the arguments described in [`DRIVER_CONTRACT`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::detect::{Decision, Environment, HostPath};
use super::{Backend, MouseButton, Screenshot, ScrollDirection, png_dimensions};
use crate::config::Config;
use crate::tools::scripted::ScriptedTool;

/// The driver's tool name, and the stem of its two files.
pub const DRIVER_NAME: &str = "computer_driver";

/// What a generated driver must do. The generation prompt, the `computer`
/// manual page and docs/computer-use.md all quote this.
pub const DRIVER_CONTRACT: &str = "\
The driver is a LuaJIT scripted tool: ~/.wizard/tools/computer_driver.toml \
(name = \"computer_driver\", script = \"computer_driver.lua\", runtime = \
\"luajit\") and ~/.wizard/tools/computer_driver.lua. Wizard calls it once per \
action with the global `args` table and treats a Lua error as failure. It may \
shell out with os.execute or io.popen. Actions:
- {action=\"screenshot\", path=<file>}: write a PNG of the whole screen to \
path. Print nothing.
- {action=\"move\", x=, y=}: move the pointer to screen pixel (x, y).
- {action=\"click\", button=\"left\"|\"right\"|\"middle\", count=n}: click at \
the current pointer position n times.
- {action=\"drag\", x=, y=}: press the left button where the pointer is, \
move to (x, y), release.
- {action=\"type\", text=}: type the text.
- {action=\"key\", chord=}: press a chord such as \"ctrl+c\", \"Return\", \
\"alt+Tab\", \"F5\" (modifiers ctrl, shift, alt, super).
- {action=\"scroll\", direction=\"up\"|\"down\"|\"left\"|\"right\", amount=n}.
- {action=\"cursor\"}: print \"x,y\", or error if the system cannot tell.
Coordinates are real screen pixels with the origin top-left, the same space \
the screenshot is in.";

/// Where the driver's manifest is expected.
pub fn manifest_path() -> Result<PathBuf> {
    Ok(Config::scripted_tools_dir()?.join(format!("{DRIVER_NAME}.toml")))
}

/// Runs the generated driver.
pub(crate) struct ScriptedBackend {
    manifest: Result<PathBuf>,
}

impl ScriptedBackend {
    pub(crate) fn new() -> Self {
        Self {
            manifest: manifest_path(),
        }
    }

    #[cfg(test)]
    fn at(manifest: PathBuf) -> Self {
        Self {
            manifest: Ok(manifest),
        }
    }

    fn call(&self, args: Value) -> Result<String> {
        let manifest = self
            .manifest
            .as_ref()
            .map_err(|err| anyhow!("no scripted tools directory: {err:#}"))?;
        if !manifest.is_file() {
            bail!(
                "the generated driver {} does not exist. Run `wizard computer setup` to generate \
                 it, or set [computer] driver = \"native\".",
                manifest.display()
            );
        }
        let tool = ScriptedTool::load(manifest)?;
        let is_lua = crate::tools::lua::is_luajit_tool(
            &tool.script_path,
            tool.manifest.runtime.as_deref(),
            tool.manifest.interpreter.as_deref(),
        );
        if !is_lua {
            bail!(
                "{} must be a LuaJIT tool (runtime = \"luajit\" or a .lua script)",
                manifest.display()
            );
        }
        let script = std::fs::read_to_string(&tool.script_path)
            .with_context(|| format!("reading {}", tool.script_path.display()))?;
        let cwd = tool
            .script_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let timeout = Duration::from_secs(tool.manifest.timeout_secs.unwrap_or(30));
        let out = crate::tools::lua::run_scripted(
            DRIVER_NAME,
            &script,
            &tool.script_path,
            &args,
            &cwd,
            timeout,
        )
        .map_err(anyhow::Error::new)?;
        if out.is_error {
            bail!("computer_driver: {}", out.content.trim());
        }
        Ok(out.content)
    }
}

fn button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

impl Backend for ScriptedBackend {
    fn label(&self) -> String {
        "host: generated driver ~/.wizard/tools/computer_driver.lua".into()
    }

    fn screenshot(&self) -> Result<Screenshot> {
        let dir = crate::platform::paths::staging_dir("computer")?;
        let path = dir.join(format!("shot-{}.png", std::process::id()));
        let _ = std::fs::remove_file(&path);
        self.call(json!({ "action": "screenshot", "path": path.to_string_lossy() }))?;
        let png = std::fs::read(&path).with_context(|| {
            format!(
                "computer_driver reported success but wrote no screenshot to {}",
                path.display()
            )
        })?;
        let _ = std::fs::remove_file(&path);
        let (width, height) = png_dimensions(&png)
            .ok_or_else(|| anyhow!("computer_driver wrote a file that is not a PNG"))?;
        Ok(Screenshot { png, width, height })
    }

    fn mouse_move(&self, x: i32, y: i32) -> Result<()> {
        self.call(json!({ "action": "move", "x": x, "y": y }))
            .map(drop)
    }

    fn click(&self, button: MouseButton, count: u32) -> Result<()> {
        self.call(json!({ "action": "click", "button": button_name(button), "count": count }))
            .map(drop)
    }

    fn drag(&self, x: i32, y: i32) -> Result<()> {
        self.call(json!({ "action": "drag", "x": x, "y": y }))
            .map(drop)
    }

    fn type_text(&self, text: &str) -> Result<()> {
        self.call(json!({ "action": "type", "text": text }))
            .map(drop)
    }

    fn key(&self, chord: &str) -> Result<()> {
        self.call(json!({ "action": "key", "chord": chord }))
            .map(drop)
    }

    fn scroll(&self, direction: ScrollDirection, amount: u32) -> Result<()> {
        let direction = format!("{direction:?}").to_ascii_lowercase();
        self.call(json!({ "action": "scroll", "direction": direction, "amount": amount }))
            .map(drop)
    }

    fn cursor_position(&self) -> Result<(i32, i32)> {
        let out = self.call(json!({ "action": "cursor" }))?;
        let (x, y) = out
            .trim()
            .split_once(',')
            .ok_or_else(|| anyhow!("computer_driver printed {out:?}, not \"x,y\""))?;
        Ok((x.trim().parse()?, y.trim().parse()?))
    }
}

/// The prompt that asks the agent to write the driver for this system.
pub fn generation_prompt(env: &Environment, decision: &Decision) -> String {
    let (why, plan) = match &decision.host {
        HostPath::Generate { why, plan } => (why.clone(), plan.clone()),
        other => (
            format!("the user asked for a generated driver ({other:?})"),
            Vec::new(),
        ),
    };
    let plan = if plan.is_empty() {
        "- pick capture and input tools that work on this system".to_string()
    } else {
        plan.iter()
            .map(|step| format!("- {step}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Write Wizard's computer-use driver for this machine.\n\n\
         System: {system}.\n\
         Why the built-in driver does not cover it: {why}.\n\n\
         Plan:\n{plan}\n\n\
         Contract:\n{DRIVER_CONTRACT}\n\n\
         Steps:\n\
         1. Check which of the tools in the plan are actually installed (`command -v`). If \
         one is missing and installing it needs root, say exactly what to install and stop.\n\
         2. Write both files with write_file. Keep the Lua short: build each command line, run \
         it, and error() with the command's output when it fails.\n\
         3. Test it the way Wizard will: run `wizard computer check --driver scripted` with the \
         execute tool. It takes a screenshot and nudges the pointer by one pixel and back. Fix \
         and repeat until it passes.\n\
         Do not click, type or open anything while testing beyond that check.",
        system = env.summary(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::computer::detect::{self, Desktop, Os, Session};

    fn write_driver(dir: &Path, lua: &str) -> PathBuf {
        std::fs::write(
            dir.join("computer_driver.toml"),
            "name = \"computer_driver\"\ndescription = \"test\"\nscript = \
             \"computer_driver.lua\"\nruntime = \"luajit\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("computer_driver.lua"), lua).unwrap();
        dir.join("computer_driver.toml")
    }

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wizard-driver-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The backend passes the contract's arguments and reads the PNG back.
    #[test]
    fn the_scripted_backend_speaks_the_contract() {
        let dir = scratch("contract");
        let log = dir.join("calls.log");
        // A 1x1 PNG, written by the "driver" on screenshot; every call is
        // appended to a log so the arguments can be checked.
        let png = "\\137PNG\\r\\n\\026\\n\\0\\0\\0\\rIHDR\\0\\0\\0\\3\\0\\0\\0\\2\\8\\2\\0\\0\\0";
        let lua = format!(
            r#"
local f = io.open("{log}", "a")
f:write(wizard.json_encode(args), "\n")
f:close()
if args.action == "screenshot" then
  local out = io.open(args.path, "wb")
  out:write("{png}")
  out:close()
elseif args.action == "cursor" then
  print("12,34")
elseif args.action == "key" and args.chord == "bad" then
  error("no such key")
end
"#,
            log = log.display()
        );
        let backend = ScriptedBackend::at(write_driver(&dir, &lua));

        let shot = backend.screenshot().expect("screenshot");
        assert_eq!((shot.width, shot.height), (3, 2));
        backend.mouse_move(5, 6).unwrap();
        backend.click(MouseButton::Right, 2).unwrap();
        backend.scroll(ScrollDirection::Down, 3).unwrap();
        assert_eq!(backend.cursor_position().unwrap(), (12, 34));
        let err = backend
            .key("bad")
            .expect_err("a Lua error fails the action");
        assert!(format!("{err:#}").contains("no such key"), "{err:#}");

        let calls = std::fs::read_to_string(&log).unwrap();
        let calls: Vec<Value> = calls
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(calls[1]["action"], "move");
        assert_eq!(calls[1]["x"], 5);
        assert_eq!(calls[2]["button"], "right");
        assert_eq!(calls[2]["count"], 2);
        assert_eq!(calls[3]["direction"], "down");
        assert_eq!(calls[3]["amount"], 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_driver_says_how_to_make_one() {
        let dir = scratch("missing");
        let backend = ScriptedBackend::at(dir.join("computer_driver.toml"));
        let err = backend.mouse_move(1, 1).unwrap_err();
        assert!(
            format!("{err:#}").contains("wizard computer setup"),
            "{err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_generation_prompt_carries_the_plan_and_the_contract() {
        let env = Environment {
            os: Os::Linux,
            wsl: false,
            session: Session::Wayland,
            desktop: Desktop::Kde,
            nixos: false,
            binaries: Default::default(),
        };
        let prompt = generation_prompt(&env, &detect::decide(&env));
        assert!(prompt.contains("KDE Plasma"), "{prompt}");
        assert!(prompt.contains("spectacle"), "{prompt}");
        assert!(prompt.contains("computer_driver.lua"), "{prompt}");
        assert!(prompt.contains("wizard computer check --driver scripted"));
    }
}
