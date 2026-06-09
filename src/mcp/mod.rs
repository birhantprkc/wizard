//! MCP client: connects to external Model Context Protocol servers
//! (stdio or HTTP transport) declared in `~/.wizard/mcp.toml`, lists their
//! tools, and exposes each as a [`Tool`] for the unified registry.
//!
//! This is the supported path for computer use, browser control, database
//! access, and any capability shipped as an MCP server — no rebuild needed.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::tools::{Tool, ToolContext, ToolError, ToolKind, ToolOutput};

/// MCP protocol revision this client speaks.
const PROTOCOL_VERSION: &str = "2025-03-26";

/// Budget for spawning/dialing a server and completing `initialize`.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Budget for one `tools/list` page.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Budget for one `tools/call`.
const CALL_TIMEOUT_SECS: u64 = 120;
/// How long to wait for a stdio child to exit before giving up on it.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Hard cap on `tools/list` pagination so a misbehaving server that keeps
/// returning cursors cannot spin us forever.
const MAX_LIST_PAGES: usize = 64;
/// Budget for one stdio JSON-RPC round trip, so a server that answers with
/// the wrong ids (or nothing at all) cannot wedge a request forever. Matches
/// the `tools/call` budget — the longest legitimate operation.
const STDIO_REQUEST_TIMEOUT: Duration = Duration::from_secs(CALL_TIMEOUT_SECS);
/// Stale/mismatched-id responses tolerated per request before we give up on
/// the server instead of reading its stdout to EOF.
const MAX_STALE_RESPONSES: usize = 50;

/// Parent environment variables forwarded to a spawned stdio server. The
/// child's environment is otherwise cleared so servers don't inherit API
/// keys and other secrets from the wizard process.
const STDIO_ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "LANG", "LC_ALL", "TERM", "USER", "SHELL", "TMPDIR",
];

/// Dynamic-linker variables never forwarded from `mcp.toml` to a stdio
/// child: each one is a code-injection vector into the spawned process.
const STDIO_ENV_DENYLIST: &[&str] = &[
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "LD_AUDIT",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
];

/// Native tool names an MCP tool must never shadow in the registry; a tool
/// advertised under one of these is namespaced `server__tool`.
const RESERVED_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "list_files",
    "search_files",
    "execute",
    "git_status",
    "git_diff",
    "spawn_subagent",
];

/// Contents of `~/.wizard/mcp.toml`:
///
/// ```toml
/// [[server]]
/// name = "computer-use"
/// transport = "stdio"
/// command = "uvx"
/// args = ["mcp-computer-use"]
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpConfig {
    #[serde(default, rename = "server")]
    pub servers: Vec<McpServerConfig>,
}

impl McpConfig {
    /// Load from `path`, returning an empty config when the file is missing.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw)
                .with_context(|| format!("invalid MCP config at {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => {
                Err(err).with_context(|| format!("failed to read MCP config at {}", path.display()))
            }
        }
    }

    /// Persist to `path` (used by `/evolve` when registering a server).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("failed to serialize MCP config")?;
        std::fs::write(path, raw)
            .with_context(|| format!("failed to write MCP config to {}", path.display()))
    }
}

/// Transport used to reach an MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// Spawn `command args...` and speak JSON-RPC over stdin/stdout.
    Stdio,
    /// Streamable HTTP endpoint at `url`.
    Http,
}

/// One `[[server]]` entry in `mcp.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// Unique server name; used to namespace colliding tool names.
    pub name: String,
    pub transport: McpTransport,
    /// Executable to spawn (stdio transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments for `command` (stdio transport).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Endpoint URL (http transport).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Extra environment variables for the spawned process (stdio transport).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
}

/// A tool as advertised by an MCP server's `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolInfo {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments (`inputSchema` on the wire).
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// Pipes of a spawned stdio server. Held under one lock so each JSON-RPC
/// request writes its line and reads its response without interleaving.
struct StdioIo {
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

/// Stdio transport state. The child handle lives behind its own lock so
/// shutdown never contends with an in-flight request holding the I/O lock.
struct StdioTransport {
    child: Mutex<Child>,
    io: Mutex<StdioIo>,
}

/// Streamable-HTTP transport state.
struct HttpTransport {
    client: reqwest::Client,
    url: String,
    /// `Mcp-Session-Id` issued by the server on `initialize`, echoed on
    /// subsequent requests.
    session_id: Mutex<Option<String>>,
}

enum Transport {
    Stdio(Box<StdioTransport>),
    Http(HttpTransport),
}

/// Live connection to one MCP server. Internally serializes JSON-RPC
/// requests; safe to share via `Arc`.
pub struct McpConnection {
    config: McpServerConfig,
    transport: Transport,
    next_id: AtomicU64,
}

impl McpConnection {
    /// Spawn/dial the server and run the MCP `initialize` handshake.
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        let transport = match config.transport {
            McpTransport::Stdio => Transport::Stdio(Box::new(spawn_stdio(&config)?)),
            McpTransport::Http => Transport::Http(open_http(&config)?),
        };
        let connection = Self {
            config,
            transport,
            next_id: AtomicU64::new(1),
        };
        timeout(CONNECT_TIMEOUT, connection.initialize())
            .await
            .map_err(|_| {
                anyhow!(
                    "MCP server '{}' did not complete initialize within {}s",
                    connection.config.name,
                    CONNECT_TIMEOUT.as_secs()
                )
            })??;
        Ok(connection)
    }

    /// Server name from its config.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// MCP `initialize` request followed by the `notifications/initialized`
    /// notification.
    async fn initialize(&self) -> Result<()> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {
                "name": "wizard",
                "version": env!("CARGO_PKG_VERSION"),
            },
        });
        let result = self.request("initialize", params).await?;
        let server_info = result
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let proto = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        debug!(
            server = %self.config.name,
            server_info,
            protocol = proto,
            "MCP initialize complete"
        );
        // A server that accepted initialize but rejects the initialized
        // notification will surface a clear error on the first real request,
        // so this is warn-and-continue.
        if let Err(err) = self.notify("notifications/initialized", json!({})).await {
            warn!(
                server = %self.config.name,
                "failed to send initialized notification: {err:#}"
            );
        }
        Ok(())
    }

    /// `tools/list` — enumerate the server's tools (follows pagination).
    pub async fn list_tools(&self) -> Result<Vec<McpToolInfo>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = timeout(LIST_TIMEOUT, self.request("tools/list", params))
                .await
                .map_err(|_| {
                    anyhow!(
                        "tools/list on MCP server '{}' timed out after {}s",
                        self.config.name,
                        LIST_TIMEOUT.as_secs()
                    )
                })??;
            let page: Vec<McpToolInfo> =
                serde_json::from_value(result.get("tools").cloned().unwrap_or_else(|| json!([])))
                    .with_context(|| {
                    format!(
                        "MCP server '{}' returned a malformed tools/list result",
                        self.config.name
                    )
                })?;
            tools.extend(page);
            match result.get("nextCursor").and_then(Value::as_str) {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => return Ok(tools),
            }
        }
        warn!(
            server = %self.config.name,
            "tools/list pagination exceeded {MAX_LIST_PAGES} pages; truncating"
        );
        Ok(tools)
    }

    /// `tools/call` — invoke a tool; returns the concatenated text content
    /// and whether the server flagged the result as an error.
    pub async fn call_tool(&self, name: &str, args: Value) -> Result<(String, bool)> {
        let arguments = match args {
            Value::Null => json!({}),
            Value::Object(map) => Value::Object(map),
            other => bail!(
                "tool arguments must be a JSON object, got {}",
                json_type_name(&other)
            ),
        };
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok((flatten_content(&result), is_error))
    }

    /// Shut down the connection (terminate the child process if stdio).
    pub async fn close(self) -> Result<()> {
        self.shutdown().await;
        Ok(())
    }

    /// Best-effort teardown usable through a shared reference (the manager
    /// calls this on `/reload` while tools may still hold `Arc`s).
    async fn shutdown(&self) {
        if let Transport::Stdio(transport) = &self.transport {
            let mut child = transport.child.lock().await;
            if let Err(err) = child.start_kill() {
                debug!(
                    server = %self.config.name,
                    "failed to signal MCP server child: {err}"
                );
            }
            if timeout(SHUTDOWN_GRACE, child.wait()).await.is_err() {
                warn!(
                    server = %self.config.name,
                    "MCP server child did not exit within {}s",
                    SHUTDOWN_GRACE.as_secs()
                );
            }
        }
    }

    /// Send one JSON-RPC request and wait for its matching response.
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        match &self.transport {
            Transport::Stdio(transport) => self.stdio_request(transport, id, &message).await,
            Transport::Http(transport) => self.http_request(transport, id, &message).await,
        }
    }

    /// Send a JSON-RPC notification (no response expected).
    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match &self.transport {
            Transport::Stdio(transport) => {
                let mut io = transport.io.lock().await;
                write_line(&mut io.stdin, &message, &self.config.name).await
            }
            Transport::Http(transport) => {
                let response = self
                    .http_post(transport, &message)
                    .await?
                    .error_for_status()
                    .with_context(|| {
                        format!(
                            "MCP server '{}' rejected notification '{}'",
                            self.config.name, method
                        )
                    })?;
                // Notifications get 202 Accepted with no body; drain anyway.
                let _ = response.bytes().await;
                Ok(())
            }
        }
    }

    /// One write-then-read round trip over the child's pipes. Skips
    /// notifications, junk lines, and (a bounded number of) stale responses;
    /// politely refuses server-to-client requests. The whole round trip is
    /// capped at [`STDIO_REQUEST_TIMEOUT`].
    async fn stdio_request(
        &self,
        transport: &StdioTransport,
        id: u64,
        message: &Value,
    ) -> Result<Value> {
        let name = &self.config.name;
        let mut io = transport.io.lock().await;
        write_line(&mut io.stdin, message, name).await?;
        timeout(
            STDIO_REQUEST_TIMEOUT,
            read_stdio_response(&mut io, id, name),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "MCP server '{name}' did not answer within {}s",
                STDIO_REQUEST_TIMEOUT.as_secs()
            )
        })?
    }

    /// One streamable-HTTP round trip: POST the request, then read the
    /// response from either a plain JSON body or an SSE stream.
    async fn http_request(
        &self,
        transport: &HttpTransport,
        id: u64,
        message: &Value,
    ) -> Result<Value> {
        let name = &self.config.name;
        let response = self.http_post(transport, message).await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!(
                "MCP server '{name}' returned HTTP {status}: {}",
                truncate(&body, 500)
            );
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();

        if content_type.starts_with("text/event-stream") {
            return self.read_sse_response(response, id).await;
        }

        let msg: Value = response
            .json()
            .await
            .with_context(|| format!("MCP server '{name}' returned a malformed JSON body"))?;
        if !id_matches(msg.get("id"), id) {
            bail!("MCP server '{name}' responded with a mismatched request id");
        }
        extract_result(msg, name)
    }

    /// POST one JSON-RPC message with MCP headers, recording any session id
    /// the server issues.
    async fn http_post(
        &self,
        transport: &HttpTransport,
        message: &Value,
    ) -> Result<reqwest::Response> {
        let name = &self.config.name;
        let mut request = transport
            .client
            .post(&transport.url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header("MCP-Protocol-Version", PROTOCOL_VERSION)
            .json(message);
        if let Some(session) = transport.session_id.lock().await.clone() {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("failed to reach MCP server '{name}' at {}", transport.url))?;
        if let Some(session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|value| value.to_str().ok())
        {
            *transport.session_id.lock().await = Some(session.to_string());
        }
        Ok(response)
    }

    /// Read SSE events from a streamable-HTTP response until the JSON-RPC
    /// response matching `id` arrives.
    async fn read_sse_response(&self, response: reqwest::Response, id: u64) -> Result<Value> {
        let name = &self.config.name;
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut data = String::new();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.with_context(|| format!("SSE stream from MCP server '{name}' failed"))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(newline) = buffer.find('\n') {
                let raw: String = buffer.drain(..=newline).collect();
                let line = raw.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    // Blank line terminates one SSE event.
                    if !data.is_empty() {
                        if let Ok(msg) = serde_json::from_str::<Value>(&data) {
                            if msg.get("method").is_none() && id_matches(msg.get("id"), id) {
                                return extract_result(msg, name);
                            }
                        } else {
                            warn!(
                                server = %name,
                                "ignoring malformed SSE data from MCP server"
                            );
                        }
                        data.clear();
                    }
                } else if let Some(payload) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(payload.trim_start());
                }
                // `event:`, `id:`, `retry:` and comment lines are irrelevant
                // here — JSON-RPC messages ride exclusively in `data:`.
            }
        }
        bail!("SSE stream from MCP server '{name}' ended without a response")
    }
}

/// Read lines from the child's stdout until the response matching `id`
/// arrives. Skips notifications and junk lines, politely refuses
/// server-to-client requests, and gives up after [`MAX_STALE_RESPONSES`]
/// mismatched-id responses so a misbehaving server cannot spin us to EOF.
async fn read_stdio_response(io: &mut StdioIo, id: u64, name: &str) -> Result<Value> {
    let mut stale = 0usize;
    let mut line = String::new();
    loop {
        line.clear();
        let read = io
            .stdout
            .read_line(&mut line)
            .await
            .with_context(|| format!("failed reading from MCP server '{name}'"))?;
        if read == 0 {
            bail!("MCP server '{name}' closed its stdout (crashed or exited)");
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(msg) => msg,
            Err(_) => {
                warn!(
                    server = %name,
                    "ignoring non-JSON line on MCP server stdout: {}",
                    truncate(trimmed, 200)
                );
                continue;
            }
        };
        if msg.get("method").is_some() {
            if let Some(req_id) = msg.get("id") {
                // Server-to-client request (sampling, roots, ...): not
                // supported by this minimal client; answer so the server
                // doesn't hang waiting on us.
                let refusal = json!({
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "error": {
                        "code": -32601,
                        "message": "method not supported by wizard",
                    },
                });
                write_line(&mut io.stdin, &refusal, name).await?;
            }
            // Plain notification: ignore.
            continue;
        }
        if !id_matches(msg.get("id"), id) {
            stale += 1;
            if stale > MAX_STALE_RESPONSES {
                bail!(
                    "MCP server '{name}' sent more than {MAX_STALE_RESPONSES} responses \
                     with mismatched ids; giving up on request {id}"
                );
            }
            debug!(server = %name, "ignoring stale MCP response");
            continue;
        }
        return extract_result(msg, name);
    }
}

/// Split config-supplied env vars into the set passed to the child and the
/// (sorted) names of dynamic-linker variables dropped for safety.
fn filter_config_env(env: &HashMap<String, String>) -> (HashMap<String, String>, Vec<String>) {
    let mut allowed = HashMap::with_capacity(env.len());
    let mut denied = Vec::new();
    for (key, value) in env {
        if STDIO_ENV_DENYLIST.contains(&key.as_str()) {
            denied.push(key.clone());
        } else {
            allowed.insert(key.clone(), value.clone());
        }
    }
    denied.sort_unstable();
    (allowed, denied)
}

/// Spawn the stdio child process for `config` (does not handshake). The
/// child's environment is cleared down to [`STDIO_ENV_ALLOWLIST`] plus the
/// config-supplied vars (minus [`STDIO_ENV_DENYLIST`]).
fn spawn_stdio(config: &McpServerConfig) -> Result<StdioTransport> {
    let command = config.command.as_deref().ok_or_else(|| {
        anyhow!(
            "MCP server '{}' uses stdio transport but has no `command`",
            config.name
        )
    })?;
    let program = shellexpand::tilde(command).into_owned();
    let (env, denied) = filter_config_env(&config.env);
    for variable in &denied {
        warn!(
            server = %config.name,
            variable = %variable,
            "dropping dynamic-linker environment variable from MCP server config"
        );
    }
    let mut command = Command::new(&program);
    command
        .args(&config.args)
        .env_clear()
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for key in STDIO_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command.envs(&env);
    let mut child = command.spawn().with_context(|| {
        format!(
            "failed to spawn MCP server '{}' (command: {program})",
            config.name
        )
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("MCP server '{}' child has no stdin pipe", config.name))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("MCP server '{}' child has no stdout pipe", config.name))?;
    if let Some(stderr) = child.stderr.take() {
        let server = config.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(server = %server, "mcp stderr: {line}");
            }
        });
    }

    Ok(StdioTransport {
        child: Mutex::new(child),
        io: Mutex::new(StdioIo {
            stdin,
            stdout: BufReader::new(stdout),
        }),
    })
}

/// Build the HTTP transport for `config` (does not handshake).
fn open_http(config: &McpServerConfig) -> Result<HttpTransport> {
    let url = config.url.clone().ok_or_else(|| {
        anyhow!(
            "MCP server '{}' uses http transport but has no `url`",
            config.name
        )
    })?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(CALL_TIMEOUT_SECS))
        .build()
        .context("failed to build HTTP client for MCP")?;
    Ok(HttpTransport {
        client,
        url,
        session_id: Mutex::new(None),
    })
}

/// Write one newline-delimited JSON-RPC message to the child's stdin.
async fn write_line(stdin: &mut ChildStdin, message: &Value, server: &str) -> Result<()> {
    let mut payload =
        serde_json::to_vec(message).context("failed to serialize JSON-RPC message")?;
    payload.push(b'\n');
    stdin
        .write_all(&payload)
        .await
        .with_context(|| format!("failed writing to MCP server '{server}' (it may have exited)"))?;
    stdin
        .flush()
        .await
        .with_context(|| format!("failed flushing write to MCP server '{server}'"))
}

/// True when a JSON-RPC `id` field matches the id we issued. Servers must
/// echo the same type, but a stringified number is tolerated.
fn id_matches(id: Option<&Value>, expected: u64) -> bool {
    match id {
        Some(Value::Number(n)) => n.as_u64() == Some(expected),
        Some(Value::String(s)) => s.parse::<u64>().is_ok_and(|v| v == expected),
        _ => false,
    }
}

/// Pull `result` out of a JSON-RPC response, converting `error` into a
/// readable failure.
fn extract_result(mut msg: Value, server: &str) -> Result<Value> {
    if let Some(error) = msg.get("error") {
        let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        bail!("MCP server '{server}' returned JSON-RPC error {code}: {message}");
    }
    Ok(msg
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

/// Flatten a `tools/call` result's content blocks into one text payload for
/// the model. Non-text blocks become readable placeholders.
fn flatten_content(result: &Value) -> String {
    let blocks = result
        .get("content")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let mut parts: Vec<String> = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                parts.push(
                    block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                );
            }
            Some("image") | Some("audio") => {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("media");
                let mime = block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type");
                parts.push(format!("[{kind} content: {mime}]"));
            }
            Some("resource") => {
                let resource = block.get("resource");
                if let Some(text) = resource.and_then(|r| r.get("text")).and_then(Value::as_str) {
                    parts.push(text.to_string());
                } else {
                    let uri = resource
                        .and_then(|r| r.get("uri"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    parts.push(format!("[binary resource: {uri}]"));
                }
            }
            Some("resource_link") => {
                let uri = block
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                parts.push(format!("[resource link: {uri}]"));
            }
            _ => {
                // Unknown block type: pass it through verbatim rather than
                // silently dropping information.
                parts.push(serde_json::to_string(block).unwrap_or_default());
            }
        }
    }
    let text = parts.join("\n");
    if text.is_empty() {
        // Some servers return only structured output.
        if let Some(structured) = result.get("structuredContent") {
            return serde_json::to_string_pretty(structured).unwrap_or_default();
        }
    }
    text
}

/// Cap `s` at `max` characters for error messages and logs.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Owns all MCP connections for a run. Built at startup and rebuilt on
/// `/reload`.
pub struct McpManager {
    connections: Vec<Arc<McpConnection>>,
}

impl McpManager {
    /// Connect to every configured server. Servers that fail to connect are
    /// skipped with a warning so one bad server doesn't take down startup.
    pub async fn connect_all(config: &McpConfig) -> Result<Self> {
        let mut connections = Vec::with_capacity(config.servers.len());
        let mut seen_names: HashSet<&str> = HashSet::new();
        for server in &config.servers {
            if !seen_names.insert(server.name.as_str()) {
                warn!(
                    server = %server.name,
                    "duplicate MCP server name in mcp.toml; skipping later entry"
                );
                continue;
            }
            match McpConnection::connect(server.clone()).await {
                Ok(connection) => {
                    debug!(server = %server.name, "connected to MCP server");
                    connections.push(Arc::new(connection));
                }
                Err(err) => {
                    warn!(
                        server = %server.name,
                        "skipping MCP server (failed to connect): {err:#}"
                    );
                }
            }
        }
        Ok(Self { connections })
    }

    /// An empty manager (no servers configured).
    pub fn empty() -> Self {
        Self {
            connections: Vec::new(),
        }
    }

    /// List the tools of every connected server as registry-ready [`Tool`]
    /// objects. Tool names colliding across servers (or with native tools)
    /// are namespaced `server__tool`.
    pub async fn tools(&self) -> Result<Vec<Arc<dyn Tool>>> {
        let mut listings: Vec<(Arc<McpConnection>, Vec<McpToolInfo>)> = Vec::new();
        for connection in &self.connections {
            match connection.list_tools().await {
                Ok(infos) => listings.push((Arc::clone(connection), infos)),
                Err(err) => {
                    warn!(
                        server = %connection.name(),
                        "skipping MCP server tools (tools/list failed): {err:#}"
                    );
                }
            }
        }

        // Count plain names across all servers so collisions get namespaced.
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        for (_, infos) in &listings {
            for info in infos {
                *name_counts.entry(info.name.clone()).or_default() += 1;
            }
        }

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut registered: HashSet<String> = HashSet::new();
        for (connection, infos) in listings {
            for info in infos {
                let collides = name_counts.get(info.name.as_str()).copied().unwrap_or(0) > 1
                    || RESERVED_TOOL_NAMES.contains(&info.name.as_str());
                let registered_name = if collides {
                    format!("{}__{}", connection.name(), info.name)
                } else {
                    info.name.clone()
                };
                if !registered.insert(registered_name.clone()) {
                    warn!(
                        server = %connection.name(),
                        tool = %info.name,
                        "duplicate MCP tool name even after namespacing; skipping"
                    );
                    continue;
                }
                tools.push(Arc::new(McpTool {
                    connection: Arc::clone(&connection),
                    info,
                    registered_name,
                }));
            }
        }
        Ok(tools)
    }

    /// Drop all connections and reconnect from `config` (for `/reload`).
    pub async fn reload(&mut self, config: &McpConfig) -> Result<()> {
        for connection in self.connections.drain(..) {
            // Tools in the (about-to-be-rebuilt) registry may still hold
            // Arcs, so tear down through the shared reference; the child is
            // also `kill_on_drop` as a backstop.
            connection.shutdown().await;
        }
        *self = Self::connect_all(config).await?;
        Ok(())
    }
}

/// Adapter exposing one remote MCP tool through the [`Tool`] trait.
pub struct McpTool {
    connection: Arc<McpConnection>,
    info: McpToolInfo,
    /// Registry-unique name (possibly namespaced `server__tool`).
    registered_name: String,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.registered_name
    }

    fn description(&self) -> &str {
        self.info.description.as_deref().unwrap_or("MCP tool")
    }

    fn parameters(&self) -> Value {
        if self.info.input_schema.is_object() {
            self.info.input_schema.clone()
        } else {
            // Servers may omit inputSchema; advertise an empty object schema
            // so the model still emits valid calls.
            json!({ "type": "object", "properties": {} })
        }
    }

    fn requires_approval(&self) -> bool {
        true
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Mcp
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if !(args.is_object() || args.is_null()) {
            return Err(ToolError::InvalidArgs {
                tool: self.registered_name.clone(),
                message: format!("expected a JSON object, got {}", json_type_name(&args)),
            });
        }
        let call = self.connection.call_tool(&self.info.name, args);
        match timeout(Duration::from_secs(CALL_TIMEOUT_SECS), call).await {
            Ok(Ok((content, true))) => Ok(ToolOutput::error(content)),
            Ok(Ok((content, false))) => Ok(ToolOutput::ok(content)),
            Ok(Err(err)) => Err(ToolError::Execution {
                tool: self.registered_name.clone(),
                source: err,
            }),
            Err(_) => Err(ToolError::Timeout {
                tool: self.registered_name.clone(),
                seconds: CALL_TIMEOUT_SECS,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_load_missing_file_is_empty() {
        let config = McpConfig::load(Path::new("/nonexistent/wizard-mcp.toml"))
            .expect("missing file should yield default config");
        assert!(config.servers.is_empty());
    }

    #[test]
    fn config_save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wizard-mcp-test-{}", std::process::id()));
        let path = dir.join("mcp.toml");
        let config = McpConfig {
            servers: vec![
                McpServerConfig {
                    name: "computer-use".into(),
                    transport: McpTransport::Stdio,
                    command: Some("uvx".into()),
                    args: vec!["mcp-computer-use".into()],
                    url: None,
                    env: HashMap::from([("FOO".to_string(), "bar".to_string())]),
                },
                McpServerConfig {
                    name: "search".into(),
                    transport: McpTransport::Http,
                    command: None,
                    args: vec![],
                    url: Some("http://127.0.0.1:8808/mcp".into()),
                    env: HashMap::new(),
                },
            ],
        };
        config.save(&path).expect("save should succeed");
        let loaded = McpConfig::load(&path).expect("load should succeed");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(loaded.servers.len(), 2);
        assert_eq!(loaded.servers[0].name, "computer-use");
        assert_eq!(loaded.servers[0].transport, McpTransport::Stdio);
        assert_eq!(loaded.servers[0].command.as_deref(), Some("uvx"));
        assert_eq!(
            loaded.servers[0].env.get("FOO").map(String::as_str),
            Some("bar")
        );
        assert_eq!(loaded.servers[1].transport, McpTransport::Http);
        assert_eq!(
            loaded.servers[1].url.as_deref(),
            Some("http://127.0.0.1:8808/mcp")
        );
    }

    #[test]
    fn id_matching_accepts_number_and_string_forms() {
        assert!(id_matches(Some(&json!(7)), 7));
        assert!(id_matches(Some(&json!("7")), 7));
        assert!(!id_matches(Some(&json!(8)), 7));
        assert!(!id_matches(Some(&json!(null)), 7));
        assert!(!id_matches(None, 7));
    }

    #[test]
    fn extract_result_surfaces_jsonrpc_errors() {
        let ok = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        assert_eq!(
            extract_result(ok, "srv").expect("result should extract"),
            json!({"tools": []})
        );

        let err = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32601, "message": "method not found"}
        });
        let message = extract_result(err, "srv")
            .expect_err("error should propagate")
            .to_string();
        assert!(message.contains("-32601"), "got: {message}");
        assert!(message.contains("method not found"), "got: {message}");
    }

    #[test]
    fn flatten_content_handles_mixed_blocks() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "mimeType": "image/png", "data": "..."},
                {"type": "resource", "resource": {"uri": "file:///a.txt", "text": "contents"}},
                {"type": "resource_link", "uri": "file:///b.txt"},
            ],
        });
        let text = flatten_content(&result);
        assert_eq!(
            text,
            "hello\n[image content: image/png]\ncontents\n[resource link: file:///b.txt]"
        );
    }

    #[test]
    fn flatten_content_falls_back_to_structured_content() {
        let result = json!({
            "content": [],
            "structuredContent": {"answer": 42},
        });
        let text = flatten_content(&result);
        assert!(text.contains("42"), "got: {text}");
    }

    /// A fake stdio MCP server implemented as a `sh` line loop: answers
    /// `initialize` (id 1), `tools/list` (id 2, one tool named `tool`), and
    /// `tools/call` (id 3).
    fn fake_server_config(name: &str, tool: &str) -> McpServerConfig {
        let script = format!(
            r#"while read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{"protocolVersion":"2025-03-26","capabilities":{{}},"serverInfo":{{"name":"fake","version":"0"}}}}}}' ;;
    *'"tools/list"'*) printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"tools":[{{"name":"{tool}","description":"a fake tool","inputSchema":{{"type":"object","properties":{{}}}}}}]}}}}' ;;
    *'"tools/call"'*) printf '%s\n' '{{"jsonrpc":"2.0","id":3,"result":{{"content":[{{"type":"text","text":"called {tool}"}}],"isError":false}}}}' ;;
  esac
done"#
        );
        McpServerConfig {
            name: name.into(),
            transport: McpTransport::Stdio,
            command: Some("sh".into()),
            args: vec!["-c".into(), script],
            url: None,
            env: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn stdio_roundtrip_namespaces_collisions_and_calls_tools() {
        let config = McpConfig {
            servers: vec![
                fake_server_config("alpha", "weather"),
                fake_server_config("beta", "weather"),
                fake_server_config("gamma", "read_file"),
            ],
        };
        let manager = McpManager::connect_all(&config)
            .await
            .expect("connect_all never hard-fails");
        let tools = manager.tools().await.expect("tools/list should succeed");
        let mut names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        names.sort_unstable();
        // `weather` collides across servers; `read_file` shadows a native.
        assert_eq!(
            names,
            vec!["alpha__weather", "beta__weather", "gamma__read_file"]
        );

        let ctx = ToolContext::new(std::env::temp_dir());
        let tool = tools
            .iter()
            .find(|tool| tool.name() == "alpha__weather")
            .expect("alpha__weather should be registered");
        assert!(tool.requires_approval());
        assert_eq!(tool.kind(), ToolKind::Mcp);
        let output = tool
            .execute(json!({}), &ctx)
            .await
            .expect("tools/call should succeed");
        assert!(!output.is_error);
        assert_eq!(output.content, "called weather");
    }

    #[tokio::test]
    async fn one_bad_server_does_not_take_down_startup() {
        let config = McpConfig {
            servers: vec![
                McpServerConfig {
                    name: "broken".into(),
                    transport: McpTransport::Stdio,
                    command: Some("/nonexistent/wizard-mcp-server".into()),
                    args: vec![],
                    url: None,
                    env: HashMap::new(),
                },
                McpServerConfig {
                    name: "misconfigured".into(),
                    transport: McpTransport::Http,
                    command: None,
                    args: vec![],
                    url: None, // http transport without a url
                    env: HashMap::new(),
                },
                fake_server_config("good", "weather"),
            ],
        };
        let manager = McpManager::connect_all(&config)
            .await
            .expect("bad servers are skipped, not fatal");
        let tools = manager.tools().await.expect("tools should list");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "weather");
    }

    #[test]
    fn filter_config_env_drops_dynamic_linker_vars() {
        let mut env: HashMap<String, String> = HashMap::from([
            ("PATH".to_string(), "/custom/bin".to_string()),
            ("API_KEY".to_string(), "secret".to_string()),
        ]);
        for key in STDIO_ENV_DENYLIST {
            env.insert((*key).to_string(), "/evil.so".to_string());
        }
        let (allowed, denied) = filter_config_env(&env);

        let mut expected_denied: Vec<String> =
            STDIO_ENV_DENYLIST.iter().map(|s| s.to_string()).collect();
        expected_denied.sort_unstable();
        assert_eq!(denied, expected_denied);
        assert_eq!(allowed.len(), 2);
        assert_eq!(allowed["PATH"], "/custom/bin");
        assert_eq!(allowed["API_KEY"], "secret");
    }

    /// End to end: the child must see config-supplied vars, but neither the
    /// denylisted linker vars nor the parent's environment (cargo always
    /// sets `CARGO_MANIFEST_DIR` for test binaries).
    #[tokio::test]
    async fn stdio_child_env_is_cleared_and_filtered() {
        let script = r#"while read -r line; do
  case "$line" in
    *'"initialize"'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"env","version":"0"}}}' ;;
    *'"env/echo"'*) printf '{"jsonrpc":"2.0","id":2,"result":{"ok":"%s","ld":"%s","manifest":"%s"}}\n' "${WIZARD_MCP_TEST_OK:-unset}" "${LD_PRELOAD:-unset}" "${CARGO_MANIFEST_DIR:-unset}" ;;
  esac
done"#;
        let connection = McpConnection::connect(McpServerConfig {
            name: "envprobe".into(),
            transport: McpTransport::Stdio,
            command: Some("sh".into()),
            args: vec!["-c".into(), script.into()],
            url: None,
            env: HashMap::from([
                ("WIZARD_MCP_TEST_OK".to_string(), "yes".to_string()),
                ("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string()),
            ]),
        })
        .await
        .expect("env probe server should connect");
        let result = connection
            .request("env/echo", json!({}))
            .await
            .expect("env/echo should answer");
        assert_eq!(result["ok"], "yes");
        assert_eq!(result["ld"], "unset");
        assert_eq!(result["manifest"], "unset");
        connection.close().await.ok();
    }

    /// A server that floods mismatched-id responses and then stalls must
    /// produce a bounded error, not read stdout until EOF or timeout.
    #[tokio::test]
    async fn stale_response_flood_fails_with_bounded_error() {
        let script = r#"read -r line
i=0
while [ "$i" -lt 60 ]; do
  printf '%s\n' '{"jsonrpc":"2.0","id":999,"result":{}}'
  i=$((i+1))
done
exec sleep 60"#;
        let result = McpConnection::connect(McpServerConfig {
            name: "flood".into(),
            transport: McpTransport::Stdio,
            command: Some("sh".into()),
            args: vec!["-c".into(), script.into()],
            url: None,
            env: HashMap::new(),
        })
        .await;
        let Err(err) = result else {
            panic!("a flood of stale responses should fail the request");
        };
        let message = format!("{err:#}");
        assert!(message.contains("flood"), "got: {message}");
        assert!(message.contains("mismatched ids"), "got: {message}");
    }

    /// A request the server never answers must hit the per-request timeout
    /// instead of hanging. Time is paused after the handshake so the 120s
    /// budget elapses instantly.
    #[tokio::test]
    async fn stdio_request_times_out_when_server_goes_silent() {
        let connection = McpConnection::connect(fake_server_config("quiet", "tool"))
            .await
            .expect("fake server should connect");
        // The fake server's `case` matches no pattern for this method and
        // prints nothing, so the client would otherwise wait forever.
        tokio::time::pause();
        let err = connection
            .request("wizard/unhandled", json!({}))
            .await
            .expect_err("an unanswered request should time out");
        tokio::time::resume();
        let message = format!("{err:#}");
        assert!(message.contains("quiet"), "got: {message}");
        assert!(message.contains("did not answer"), "got: {message}");
        connection.close().await.ok();
    }

    #[test]
    fn truncate_caps_long_strings() {
        assert_eq!(truncate("short", 10), "short");
        let long = "x".repeat(20);
        let cut = truncate(&long, 10);
        assert!(cut.starts_with("xxxxxxxxxx") && cut.ends_with('…'));
    }
}
