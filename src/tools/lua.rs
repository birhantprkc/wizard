//! In-process LuaJIT runner for self-extension.
//!
//! Scripted tools ending in `.lua` (or whose manifest sets `runtime = "luajit"`)
//! execute here instead of spawning an external interpreter. Wizard embeds
//! LuaJIT — the just-in-time compiler — so evolve glue is fast, portable, and
//! does not depend on whatever shell/Python/Node happens to be on `PATH`.
//!
//! Contract for a Lua tool script:
//! - Tool arguments arrive as a Lua table in the global `args` (decoded from
//!   the JSON object the model passed).
//! - The project root is in the global `cwd` (string).
//! - Print results with `print(...)` (captured as the tool's stdout).
//! - Raise an error (or return a string starting with `"error:"`) to fail.
//! - Returning a non-nil value is treated as the result when nothing was
//!   printed; tables/values are JSON-encoded.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Value as LuaValue};
use serde_json::Value as JsonValue;

use super::shell::{DEFAULT_TIMEOUT, MAX_TIMEOUT};
use super::{MAX_OUTPUT_BYTES, ToolError, ToolOutput, truncate_output};

/// Run a LuaJIT script for a scripted tool.
///
/// `script` is the source; `script_path` is only used in error messages.
/// `args` is the JSON object the model supplied; it becomes the global `args`.
pub fn run_scripted(
    tool: &str,
    script: &str,
    script_path: &Path,
    args: &JsonValue,
    cwd: &Path,
    timeout: Duration,
) -> Result<ToolOutput, ToolError> {
    let timeout = timeout.clamp(Duration::from_secs(1), MAX_TIMEOUT);
    let tool_name = tool.to_string();
    let script = script.to_string();
    let script_path = script_path.to_path_buf();
    let args = args.clone();
    let cwd = cwd.to_path_buf();

    // LuaJIT (and mlua's Lua handle under `send`) must not cross an await while
    // held on this thread's stack in ways that confuse the runtime; running the
    // whole chunk on a blocking pool worker keeps the agent loop free and lets
    // us enforce a wall-clock timeout with `spawn_blocking` + oneshot cancel is
    // awkward, so we use `tokio::time::timeout` around the join.
    let join = std::thread::Builder::new()
        .name(format!("wizard-luajit-{tool_name}"))
        .spawn(move || run_lua_blocking(&tool_name, &script, &script_path, &args, &cwd))
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::Error::new(err).context("failed to spawn LuaJIT worker"),
        })?;

    let start = std::time::Instant::now();
    loop {
        if join.is_finished() {
            return join
                .join()
                .unwrap_or_else(|_| {
                    Err(ToolError::Execution {
                        tool: tool.to_string(),
                        source: anyhow::anyhow!("LuaJIT worker panicked"),
                    })
                });
        }
        if start.elapsed() >= timeout {
            // The worker cannot be safely aborted mid-Lua (no kill for a
            // foreign stack). Report timeout; the OS reaps the thread when the
            // chunk eventually returns. Tools should stay short.
            return Err(ToolError::Timeout {
                tool: tool.to_string(),
                seconds: timeout.as_secs().max(1),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Async wrapper used by [`super::scripted::ScriptedTool`].
pub async fn run_scripted_async(
    tool: &str,
    script: &str,
    script_path: &Path,
    args: &JsonValue,
    cwd: &Path,
    timeout: Duration,
) -> Result<ToolOutput, ToolError> {
    let timeout = timeout.clamp(Duration::from_secs(1), MAX_TIMEOUT);
    let tool_name = tool.to_string();
    let script = script.to_string();
    let script_path = script_path.to_path_buf();
    let args = args.clone();
    let cwd = cwd.to_path_buf();

    match tokio::time::timeout(
        timeout,
        tokio::task::spawn_blocking(move || {
            run_lua_blocking(&tool_name, &script, &script_path, &args, &cwd)
        }),
    )
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(join_err)) => Err(ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::Error::new(join_err).context("LuaJIT worker panicked"),
        }),
        Err(_) => Err(ToolError::Timeout {
            tool: tool.to_string(),
            seconds: timeout.as_secs().max(1),
        }),
    }
}

fn run_lua_blocking(
    tool: &str,
    script: &str,
    script_path: &Path,
    args: &JsonValue,
    cwd: &Path,
) -> Result<ToolOutput, ToolError> {
    let lua = Lua::new_with(StdLib::ALL_SAFE, LuaOptions::default()).map_err(|err| {
        ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!("failed to create LuaJIT state: {err}"),
        }
    })?;

    // Capture print() into a shared buffer the host reads after the chunk.
    let stdout: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    {
        let buf = Arc::clone(&stdout);
        let print_fn = lua
            .create_function(move |lua, values: mlua::MultiValue| {
                let mut line = String::new();
                for (i, value) in values.into_iter().enumerate() {
                    if i > 0 {
                        line.push('\t');
                    }
                    line.push_str(&lua_value_to_string(lua, value)?);
                }
                line.push('\n');
                if let Ok(mut guard) = buf.lock() {
                    guard.push_str(&line);
                }
                Ok(())
            })
            .map_err(|err| ToolError::Execution {
                tool: tool.to_string(),
                source: anyhow::anyhow!("installing print(): {err}"),
            })?;
        lua.globals()
            .set("print", print_fn)
            .map_err(|err| ToolError::Execution {
                tool: tool.to_string(),
                source: anyhow::anyhow!("setting print: {err}"),
            })?;
    }

    // args / cwd globals.
    let args_lua = json_to_lua(&lua, args).map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::anyhow!("converting tool arguments to Lua: {err}"),
    })?;
    lua.globals()
        .set("args", args_lua)
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!("setting args: {err}"),
        })?;
    lua.globals()
        .set("cwd", cwd_string(cwd))
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!("setting cwd: {err}"),
        })?;

    // Lightweight std helpers so evolve glue does not need FFI for common work.
    install_wizard_lib(&lua, cwd).map_err(|err| ToolError::Execution {
        tool: tool.to_string(),
        source: anyhow::anyhow!("installing wizard.* helpers: {err}"),
    })?;

    let chunk_name = format!("@{}", script_path.display());
    let result = lua
        .load(script)
        .set_name(&chunk_name)
        .eval::<LuaValue>()
        .map_err(|err| ToolError::Execution {
            tool: tool.to_string(),
            source: anyhow::anyhow!(
                "LuaJIT error in {}:\n{err}",
                script_path.display()
            ),
        })?;

    let printed = stdout
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();

    let content = if !printed.is_empty() {
        printed
    } else {
        match result {
            LuaValue::Nil => String::new(),
            other => lua_value_to_json_string(&lua, other).unwrap_or_else(|err| err.to_string()),
        }
    };

    // Convention: scripts may signal soft failure by returning/printing a
    // line that starts with "error:".
    let trimmed = content.trim_start();
    let is_error = trimmed.starts_with("error:") || trimmed.starts_with("Error:");
    let content = truncate_output(content, MAX_OUTPUT_BYTES);
    if is_error {
        Ok(ToolOutput::error(content))
    } else {
        Ok(ToolOutput::ok(content))
    }
}

fn cwd_string(cwd: &Path) -> String {
    cwd.to_string_lossy().into_owned()
}

/// `wizard.read_file`, `wizard.write_file`, `wizard.json_encode/decode` —
/// small host bridge so Lua tools do real work without shelling out.
fn install_wizard_lib(lua: &Lua, cwd: &Path) -> mlua::Result<()> {
    let table = lua.create_table()?;
    let cwd_read = PathBuf::from(cwd);
    let cwd_write = PathBuf::from(cwd);

    let read_file = lua.create_function(move |_, path: String| {
        let p = resolve_against(&cwd_read, &path);
        std::fs::read_to_string(&p).map_err(mlua::Error::external)
    })?;
    table.set("read_file", read_file)?;

    let write_file = lua.create_function(move |_, (path, contents): (String, String)| {
        let p = resolve_against(&cwd_write, &path);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).map_err(mlua::Error::external)?;
        }
        std::fs::write(&p, contents).map_err(mlua::Error::external)
    })?;
    table.set("write_file", write_file)?;

    let json_encode = lua.create_function(|lua, value: LuaValue| {
        let json = lua_to_json(lua, value).map_err(mlua::Error::external)?;
        serde_json::to_string(&json).map_err(mlua::Error::external)
    })?;
    table.set("json_encode", json_encode)?;

    let json_decode = lua.create_function(|lua, raw: String| {
        let json: JsonValue = serde_json::from_str(&raw).map_err(mlua::Error::external)?;
        json_to_lua(lua, &json).map_err(mlua::Error::external)
    })?;
    table.set("json_decode", json_decode)?;

    // Identity marker so scripts (and doctors) can see they are on LuaJIT.
    table.set("runtime", "luajit")?;
    table.set(
        "version",
        lua.load("return jit and jit.version or _VERSION")
            .eval::<String>()
            .unwrap_or_else(|_| "Lua".into()),
    )?;

    lua.globals().set("wizard", table)?;
    Ok(())
}

fn resolve_against(cwd: &Path, path: &str) -> PathBuf {
    let expanded = shellexpand::tilde(path);
    let candidate = Path::new(expanded.as_ref());
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    }
}

fn json_to_lua(lua: &Lua, value: &JsonValue) -> mlua::Result<LuaValue> {
    // mlua's serde feature: Value implements Serialize/Deserialize via
    // Lua ser/de — use from_value on the JSON side through serde_json -> Lua.
    lua.to_value(value)
}

fn lua_to_json(lua: &Lua, value: LuaValue) -> mlua::Result<JsonValue> {
    lua.from_value(value)
}

fn lua_value_to_string(lua: &Lua, value: LuaValue) -> mlua::Result<String> {
    match value {
        LuaValue::Nil => Ok("nil".into()),
        LuaValue::Boolean(b) => Ok(b.to_string()),
        LuaValue::Integer(i) => Ok(i.to_string()),
        LuaValue::Number(n) => Ok(n.to_string()),
        LuaValue::String(s) => Ok(s.to_str()?.to_owned()),
        other => {
            // tostring() via Lua for tables/userdata.
            let tostring: mlua::Function = lua.globals().get("tostring")?;
            tostring.call::<String>(other)
        }
    }
}

fn lua_value_to_json_string(lua: &Lua, value: LuaValue) -> mlua::Result<String> {
    match &value {
        LuaValue::String(s) => Ok(s.to_str()?.to_owned()),
        LuaValue::Nil => Ok(String::new()),
        _ => {
            let json = lua_to_json(lua, value)?;
            Ok(serde_json::to_string_pretty(&json).unwrap_or_default())
        }
    }
}

/// True when a scripted tool should run through the embedded LuaJIT runtime.
pub fn is_luajit_tool(script_path: &Path, interpreter: Option<&str>, runtime: Option<&str>) -> bool {
    if runtime.is_some_and(|r| {
        let r = r.trim().to_ascii_lowercase();
        r == "luajit" || r == "lua" || r == "embedded"
    }) {
        return true;
    }
    if let Some(interp) = interpreter {
        let i = interp.to_ascii_lowercase();
        if i.contains("luajit") || i == "lua" || i.ends_with("/lua") || i.ends_with("/luajit") {
            return true;
        }
    }
    script_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("lua"))
}

/// Default timeout helper re-export for callers that do not want shell deps.
#[allow(dead_code)]
pub fn default_timeout() -> Duration {
    DEFAULT_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_lua_by_extension_and_runtime() {
        assert!(is_luajit_tool(Path::new("x.lua"), None, None));
        assert!(is_luajit_tool(Path::new("x.sh"), None, Some("luajit")));
        assert!(is_luajit_tool(Path::new("x.sh"), Some("luajit"), None));
        assert!(!is_luajit_tool(Path::new("x.sh"), Some("bash"), None));
    }

    #[test]
    fn runs_print_and_args() {
        let out = run_scripted(
            "t",
            r#"print("hello", args.name)"#,
            Path::new("t.lua"),
            &json!({"name": "wizard"}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("hello"), "{}", out.content);
        assert!(out.content.contains("wizard"), "{}", out.content);
    }

    #[test]
    fn return_value_used_when_silent() {
        let out = run_scripted(
            "t",
            r#"return args.n * 2"#,
            Path::new("t.lua"),
            &json!({"n": 21}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!out.is_error);
        assert!(out.content.trim() == "42" || out.content.contains("42"), "{}", out.content);
    }

    #[test]
    fn lua_error_becomes_tool_error() {
        let err = run_scripted(
            "t",
            r#"error("boom")"#,
            Path::new("t.lua"),
            &json!({}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap_err();
        let full = match &err {
            ToolError::Execution { source, .. } => format!("{err:#}\n{source:#}"),
            other => format!("{other:#}"),
        };
        assert!(
            full.contains("boom") || full.contains("LuaJIT error"),
            "{full}"
        );
    }

    #[test]
    fn wizard_json_roundtrip() {
        let out = run_scripted(
            "t",
            r#"
local enc = wizard.json_encode(args)
local dec = wizard.json_decode(enc)
print(dec.x)
print(wizard.runtime)
"#,
            Path::new("t.lua"),
            &json!({"x": "ok"}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.content.contains("ok"), "{}", out.content);
        assert!(out.content.contains("luajit"), "{}", out.content);
    }

    #[test]
    fn soft_error_prefix() {
        let out = run_scripted(
            "t",
            r#"print("error: nope")"#,
            Path::new("t.lua"),
            &json!({}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn async_wrapper_works() {
        let out = run_scripted_async(
            "t",
            r#"return "async-ok""#,
            Path::new("t.lua"),
            &json!({}),
            Path::new("."),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(out.content.contains("async-ok"), "{}", out.content);
    }
}
