//! The GUI's HTTP surface: static assets (embedded at compile time from
//! `gui/assets/`, or served from disk with `--assets`) plus the JSON API of
//! `docs/gui-protocol.md`.

use std::collections::{HashMap, HashSet};
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use axum::Router;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent::session::{self, Session};
use crate::commands::{CommandSpec, Execution};
use crate::config::{Config, ProviderConfig, ProviderKind};
use crate::gui::tasks::TaskState;
use crate::gui::{GuiState, git, oauth, settings, transcript, ws};
use crate::session_registry::{self, SessionState};

/// How long `/api/models` waits on one provider's model listing before
/// falling back to an empty list (the picker then shows just the configured
/// model).
const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a Settings "test provider" probe waits. Longer than
/// [`LIST_MODELS_TIMEOUT`]: the user is watching this one and wants a verdict,
/// not a shrug.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// One embedded static asset.
struct Asset {
    name: &'static str,
    mime: &'static str,
    body: &'static str,
}

/// The bundled typefaces (see `gui/assets/fonts/README.md`): Inter for the UI,
/// JetBrains Mono for literals, both variable-weight latin subsets under the
/// OFL. Embedded rather than assumed, because a machine with neither installed
/// falls back to DejaVu Sans and the GUI looks like a 1998 dialog box.
struct FontAsset {
    name: &'static str,
    body: &'static [u8],
}

const FONTS: [FontAsset; 2] = [
    FontAsset {
        name: "inter.woff2",
        body: include_bytes!("../../gui/assets/fonts/inter.woff2"),
    },
    FontAsset {
        name: "jetbrains-mono.woff2",
        body: include_bytes!("../../gui/assets/fonts/jetbrains-mono.woff2"),
    },
];

/// The GUI's assets, embedded at compile time so the binary stays
/// self-contained.
const ASSETS: [Asset; 15] = [
    Asset {
        name: "index.html",
        mime: "text/html; charset=utf-8",
        body: include_str!("../../gui/assets/index.html"),
    },
    Asset {
        name: "style.css",
        mime: "text/css; charset=utf-8",
        body: include_str!("../../gui/assets/style.css"),
    },
    Asset {
        name: "app.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/app.js"),
    },
    Asset {
        name: "api.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/api.js"),
    },
    Asset {
        name: "icons.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/icons.js"),
    },
    Asset {
        name: "dom.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/dom.js"),
    },
    Asset {
        name: "markdown.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/markdown.js"),
    },
    Asset {
        name: "transcript.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/transcript.js"),
    },
    Asset {
        name: "pane.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/pane.js"),
    },
    Asset {
        name: "context.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/context.js"),
    },
    Asset {
        name: "subagents.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/subagents.js"),
    },
    Asset {
        name: "attach.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/attach.js"),
    },
    Asset {
        name: "palette.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/palette.js"),
    },
    Asset {
        name: "composer.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/composer.js"),
    },
    Asset {
        name: "settings.js",
        mime: "text/javascript; charset=utf-8",
        body: include_str!("../../gui/assets/settings.js"),
    },
];

/// The favicon, served at `/favicon.ico` for clients that probe the classic
/// path; `index.html` inlines the same glyph as a data URI.
const FAVICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16"><path fill="#ececee" d="M8 0C8.6 4.2 11.8 7.4 16 8C11.8 8.6 8.6 11.8 8 16C7.4 11.8 4.2 8.6 0 8C4.2 7.4 7.4 4.2 8 0Z"/></svg>"##;

/// Build the GUI router over the shared state. Every route — the WebSocket
/// upgrade included — sits behind [`local_guard`].
pub(crate) fn router(state: Arc<GuiState>) -> Router {
    Router::new()
        .route("/", get(serve_asset))
        .route("/index.html", get(serve_asset))
        .route("/style.css", get(serve_asset))
        .route("/app.js", get(serve_asset))
        .route("/api.js", get(serve_asset))
        .route("/icons.js", get(serve_asset))
        .route("/dom.js", get(serve_asset))
        .route("/markdown.js", get(serve_asset))
        .route("/transcript.js", get(serve_asset))
        .route("/pane.js", get(serve_asset))
        .route("/context.js", get(serve_asset))
        .route("/subagents.js", get(serve_asset))
        .route("/attach.js", get(serve_asset))
        .route("/palette.js", get(serve_asset))
        .route("/composer.js", get(serve_asset))
        .route("/settings.js", get(serve_asset))
        .route("/fonts/{name}", get(serve_font))
        .route("/favicon.ico", get(favicon))
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/{id}", get(get_task))
        .route("/api/tasks/{id}/ws", get(task_ws))
        .route("/api/tasks/{id}/upload", post(upload))
        .route("/api/commands", get(commands))
        .route("/api/workspace", get(workspace))
        .route("/api/models", get(models))
        .route("/api/settings", get(get_settings).patch(patch_settings))
        .route("/api/login/{provider}", post(begin_sign_in))
        .route("/api/login", get(sign_in_status))
        .route("/api/providers", post(save_provider))
        .route("/api/providers/{name}", delete(delete_provider))
        .route("/api/providers/{name}/active", post(activate_provider))
        .route("/api/providers/{name}/test", post(test_provider))
        .route("/api/workspaces", get(workspaces))
        .route("/api/image", get(image))
        .route("/api/git", get(git_status))
        .route("/api/git/diff", get(git_diff))
        .route("/api/git/branches", get(git_branches))
        .route("/api/git/checkout", post(git_checkout))
        .layer(middleware::from_fn(local_guard))
        .with_state(state)
}

/// Drive-by protection for the localhost-only server: a hostile web page can
/// still reach 127.0.0.1 from the user's browser — WebSocket upgrades are
/// not subject to CORS, and DNS rebinding defeats same-origin for the plain
/// HTTP API — so every request must name a loopback `Host`, and any `Origin`
/// it carries must be a local page. Requests without an `Origin` (curl,
/// same-origin navigations) pass on the Host check alone.
async fn local_guard(request: Request, next: Next) -> Response {
    if !is_local_request(request.headers()) {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

/// The [`local_guard`] predicate: loopback `Host`, loopback `Origin` when
/// one is present.
fn is_local_request(headers: &HeaderMap) -> bool {
    let host_ok = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(host_is_local);
    let origin_ok = match headers.get(header::ORIGIN) {
        None => true,
        Some(value) => value.to_str().ok().is_some_and(origin_is_local),
    };
    host_ok && origin_ok
}

/// True for a loopback host value: `127.0.0.1`, `localhost`, or `[::1]`,
/// each with an optional `:port`.
fn host_is_local(value: &str) -> bool {
    let host = match value.strip_prefix('[') {
        // Bracketed IPv6, e.g. `[::1]:4680`.
        Some(rest) => match rest.split_once(']') {
            Some((host, port)) if port.is_empty() || port.starts_with(':') => host,
            _ => return false,
        },
        None => value.split(':').next().unwrap_or(value),
    };
    host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost")
}

/// True for a local page `Origin`: a loopback host over http(s).
fn origin_is_local(value: &str) -> bool {
    value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .is_some_and(host_is_local)
}

/// A JSON API error: `{ "error": "..." }` with the matching status.
#[derive(Debug)]
struct ApiError(StatusCode, String);

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, message.into())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, axum::Json(json!({ "error": self.1 }))).into_response()
    }
}

/// Static assets: the embedded copies, or (dev mode) the same names read
/// from the `--assets` directory on every request.
///
/// Served `no-store`. The assets carry no version in their URL, so a browser
/// that heuristically caches `/app.js` keeps showing an old build of the GUI
/// after an upgrade — which looks exactly like the new build not working. They
/// are a few KB off localhost; there is nothing to gain by caching them.
async fn serve_asset(State(state): State<Arc<GuiState>>, uri: Uri) -> Response {
    let name = match uri.path() {
        "/" | "/index.html" => "index.html",
        other => other.trim_start_matches('/'),
    };
    let Some(asset) = ASSETS.iter().find(|asset| asset.name == name) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let headers = [
        (header::CONTENT_TYPE, asset.mime),
        (header::CACHE_CONTROL, "no-store"),
    ];
    if let Some(dir) = &state.assets_dir
        && let Ok(body) = tokio::fs::read_to_string(dir.join(asset.name)).await
    {
        return (headers, body).into_response();
    }
    (headers, asset.body).into_response()
}

/// `GET /fonts/{name}`: a bundled woff2. Unlike the code assets these never
/// change within a build, so they are cached hard — the alternative is
/// re-sending 80 KB on every page load.
async fn serve_font(
    Path(name): Path<String>,
    State(state): State<Arc<GuiState>>,
) -> Result<Response, ApiError> {
    let Some(font) = FONTS.iter().find(|font| font.name == name) else {
        return Err(ApiError::not_found(format!("no font '{name}'")));
    };
    let headers = [
        (header::CONTENT_TYPE, "font/woff2"),
        (header::CACHE_CONTROL, "public, max-age=604800, immutable"),
    ];
    if let Some(dir) = &state.assets_dir
        && let Ok(body) = tokio::fs::read(dir.join("fonts").join(font.name)).await
    {
        return Ok((headers, body).into_response());
    }
    Ok((headers, font.body).into_response())
}

/// `GET /favicon.ico`: the embedded SVG sparkle (see [`FAVICON_SVG`]).
async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], FAVICON_SVG)
}

/// One sidebar row of `GET /api/tasks`.
#[derive(Debug, Serialize)]
struct TaskRow {
    id: String,
    title: String,
    cwd: String,
    workspace: String,
    updated_unix: u64,
    state: &'static str,
}

/// `GET /api/tasks`: every session on disk merged with the live registry
/// (`~/.wizard/running/`) and this server's own managed tasks, newest
/// first.
async fn list_tasks(
    State(state): State<Arc<GuiState>>,
) -> Result<axum::Json<Vec<TaskRow>>, ApiError> {
    let sessions_dir = Config::sessions_dir()?;
    let live = state.manager.states();
    let registry: HashMap<String, session_registry::SessionRecord> = session_registry::list()
        .into_iter()
        .map(|record| (record.id.clone(), record))
        .collect();

    let mut rows = Vec::new();
    let mut seen = HashSet::new();
    for summary in session::summaries(&sessions_dir) {
        let task_state = live
            .get(&summary.id)
            .map(|s| s.as_str())
            .or_else(|| registry.get(&summary.id).map(|r| registry_state(r.state)))
            .unwrap_or("done");
        let cwd = summary.cwd.clone().unwrap_or_default();
        let updated = mtime_unix(&sessions_dir.join(format!("{}.jsonl", summary.id))).unwrap_or(0);
        seen.insert(summary.id.clone());
        rows.push(TaskRow {
            workspace: basename(&cwd),
            cwd,
            id: summary.id,
            title: summary.summary,
            updated_unix: updated,
            state: task_state,
        });
    }
    // Registry entries without a session file (e.g. a foreign sessions dir)
    // still list, so nothing running on the machine is invisible.
    for record in registry.into_values() {
        if seen.contains(&record.id) {
            continue;
        }
        rows.push(TaskRow {
            workspace: basename(&record.cwd),
            title: record.name,
            cwd: record.cwd,
            updated_unix: record.updated_unix,
            state: registry_state(record.state),
            id: record.id,
        });
    }
    rows.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix).then(b.id.cmp(&a.id)));
    Ok(axum::Json(rows))
}

/// `GET /api/tasks/{id}`: the full transcript replay.
#[derive(Debug, Serialize)]
struct TaskDetail {
    id: String,
    cwd: String,
    workspace: String,
    model: String,
    items: Vec<transcript::Item>,
}

async fn get_task(
    Path(id): Path<String>,
    State(state): State<Arc<GuiState>>,
) -> Result<axum::Json<TaskDetail>, ApiError> {
    let sessions_dir = Config::sessions_dir()?;
    let session = Session::open_by_id(&sessions_dir, &id)?
        .ok_or_else(|| ApiError::not_found(format!("no task '{id}'")))?;
    let entries = session.entries()?;
    let cwd = session.cwd().unwrap_or_default().to_string();
    let model = state
        .manager
        .model_of(&id)
        .unwrap_or_else(|| state.config.current().active().model);
    Ok(axum::Json(TaskDetail {
        workspace: basename(&cwd),
        cwd,
        model,
        items: transcript::replay(&entries),
        id,
    }))
}

/// `POST /api/tasks` body. Both fields are optional: no `cwd` means the
/// directory `wizard gui` was launched from, and no `prompt` opens an empty
/// chat whose first `user_message` starts the first turn.
#[derive(Debug, Deserialize, Default)]
struct CreateTask {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

/// `POST /api/tasks`: create the session, and start the first turn when a
/// prompt came with it; the client opens the WebSocket to catch the stream
/// (frames are buffered until it attaches).
async fn create_task(
    State(state): State<Arc<GuiState>>,
    body: Option<axum::Json<CreateTask>>,
) -> Result<(StatusCode, axum::Json<Value>), ApiError> {
    let body = body.map(|axum::Json(body)| body).unwrap_or_default();
    let cwd = match &body.cwd {
        Some(cwd) => PathBuf::from(cwd),
        None => state.cwd.clone(),
    };
    if !cwd.is_absolute() || !cwd.is_dir() {
        return Err(ApiError::bad_request(format!(
            "cwd '{}' is not an absolute path to a directory",
            cwd.display()
        )));
    }
    let prompt = body.prompt.filter(|prompt| !prompt.trim().is_empty());
    let id = state.manager.create_task(&cwd, prompt, body.model)?;
    Ok((
        StatusCode::CREATED,
        axum::Json(json!({
            "id": id,
            "cwd": cwd.display().to_string(),
            "workspace": basename(&cwd.display().to_string()),
        })),
    ))
}

/// `GET /api/tasks/{id}/ws`: upgrade to the task's event stream. An unknown
/// id is a 404 before the upgrade.
async fn task_ws(
    Path(id): Path<String>,
    State(state): State<Arc<GuiState>>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let shared = state
        .manager
        .ensure(&id)
        .map_err(|err| ApiError::not_found(format!("{err:#}")))?;
    Ok(upgrade.on_upgrade(move |socket| ws::serve(socket, shared, state)))
}

/// `GET /api/workspace`: the directory `wizard gui` was launched from — the
/// one a new chat opens in.
async fn workspace(State(state): State<Arc<GuiState>>) -> axum::Json<Value> {
    let cwd = state.cwd.display().to_string();
    axum::Json(json!({ "name": basename(&cwd), "cwd": cwd }))
}

/// `GET /api/models` response.
#[derive(Debug, Serialize)]
struct ModelsResponse {
    active: String,
    providers: Vec<ProviderRow>,
}

#[derive(Debug, Serialize)]
struct ProviderRow {
    name: String,
    kind: String,
    model: String,
    models: Vec<String>,
}

/// `GET /api/models`: the configured providers with a best-effort model
/// listing each (bounded by [`LIST_MODELS_TIMEOUT`]; unreachable backends
/// report an empty list rather than an error).
async fn models(State(state): State<Arc<GuiState>>) -> axum::Json<ModelsResponse> {
    let config = state.config.current();
    let providers = if config.providers.is_empty() {
        vec![config.active()]
    } else {
        config.providers.clone()
    };
    let rows = futures_util::future::join_all(providers.iter().map(list_provider_models)).await;
    axum::Json(ModelsResponse {
        active: config.active().name,
        providers: rows,
    })
}

/// One provider's models, best-effort: an unreachable backend or a bad key
/// yields an empty list rather than failing the request.
async fn list_provider_models(provider: &ProviderConfig) -> ProviderRow {
    let models = match provider.build() {
        Ok(client) => match tokio::time::timeout(LIST_MODELS_TIMEOUT, client.list_models()).await {
            Ok(Ok(models)) => models,
            _ => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    ProviderRow {
        name: provider.name.clone(),
        kind: provider.kind.to_string(),
        model: provider.model.clone(),
        models,
    }
}

/* ---------------------------------------------------------------------- */
/* Subscription sign-in (OAuth)                                           */
/* ---------------------------------------------------------------------- */

/// `POST /api/login/{provider}`: start a sign-in and hand back the URL to send
/// the user to.
///
/// Neither redirect comes back here. A provider only redirects to the loopback
/// address registered with its client id, so each flow binds that listener
/// itself and finishes in a task of its own; this server just watches
/// [`sign_in_status`].
async fn begin_sign_in(
    Path(provider): Path<String>,
    State(state): State<Arc<GuiState>>,
) -> Result<axum::Json<Value>, ApiError> {
    let url = match provider.as_str() {
        "xai" => state.sign_in.begin_xai(Arc::clone(&state.config)).await?,
        "chatgpt" => {
            state
                .sign_in
                .begin_chatgpt(Arc::clone(&state.config))
                .await?
        }
        other => {
            return Err(ApiError::bad_request(format!(
                "cannot sign in to '{other}'"
            )));
        }
    };
    Ok(axum::Json(json!({ "authorize_url": url })))
}

/// `GET /api/login`: what the sign-in that is in flight is doing. The tab the
/// user started from polls this, because the tab they *finish* in is the
/// provider's own — which lands on the flow's private callback listener, not
/// here.
async fn sign_in_status(State(state): State<Arc<GuiState>>) -> axum::Json<oauth::Status> {
    axum::Json(state.sign_in.status())
}

/* ---------------------------------------------------------------------- */
/* Settings                                                               */
/* ---------------------------------------------------------------------- */

/// `GET /api/settings`: everything the Settings page and onboarding render.
#[derive(Debug, Serialize)]
struct SettingsResponse {
    /// No provider is configured: the GUI onboards instead of opening a chat.
    first_run: bool,
    config_path: String,
    credentials_path: Option<String>,
    active: Option<String>,
    /// Model → tool → model round trips one turn may take (`max_steps`), on
    /// every surface: the GUI is the same agent as the TUI, so it edits the same
    /// field. `0` is the default and means no limit.
    max_steps: u32,
    providers: Vec<SettingsProvider>,
    presets: Vec<settings::Preset>,
}

#[derive(Debug, Serialize)]
struct SettingsProvider {
    name: String,
    kind: String,
    base_url: String,
    model: String,
    key: settings::KeySource,
    active: bool,
}

fn settings_response(config: &Config) -> SettingsResponse {
    let active = config.active().name;
    SettingsResponse {
        first_run: config.providers.is_empty(),
        config_path: settings::config_path(),
        credentials_path: settings::credentials_path().map(|p| p.display().to_string()),
        active: config.active_provider.clone(),
        max_steps: config.max_steps.cap().unwrap_or(0),
        providers: config
            .providers
            .iter()
            .map(|provider| SettingsProvider {
                name: provider.name.clone(),
                kind: provider.kind.to_string(),
                base_url: provider.base_url.clone(),
                model: provider.model.clone(),
                key: settings::key_source(provider),
                active: provider.name == active,
            })
            .collect(),
        presets: settings::presets(),
    }
}

async fn get_settings(State(state): State<Arc<GuiState>>) -> axum::Json<SettingsResponse> {
    axum::Json(settings_response(&state.config.current()))
}

/// `PATCH /api/settings`: the step budget every surface runs on.
#[derive(Debug, Deserialize)]
struct PatchSettings {
    #[serde(default)]
    max_steps: Option<u32>,
}

async fn patch_settings(
    State(state): State<Arc<GuiState>>,
    axum::Json(body): axum::Json<PatchSettings>,
) -> Result<axum::Json<SettingsResponse>, ApiError> {
    // 0 is the default and means unlimited; the ceiling is a sanity bound on a
    // number typed into a box, not a policy.
    if let Some(steps) = body.max_steps
        && steps > 1000
    {
        return Err(ApiError::bad_request(
            "the step limit must be 0 (no limit) or at most 1000",
        ));
    }
    let config = state.config.update(|config| {
        if let Some(steps) = body.max_steps {
            config.max_steps = crate::config::StepBudget::new(steps);
        }
        Ok(())
    })?;
    Ok(axum::Json(settings_response(&config)))
}

/// `POST /api/providers` body: add a provider, or edit one by reusing its
/// name. An `api_key` is stored in `~/.wizard/credentials.toml`; omitting it
/// on an edit keeps the key already there.
#[derive(Debug, Deserialize)]
struct SaveProvider {
    name: String,
    kind: String,
    base_url: String,
    model: String,
    #[serde(default)]
    api_key: Option<String>,
    /// Make this the active provider (default: yes — you configured it to use it).
    #[serde(default = "yes")]
    activate: bool,
}

fn yes() -> bool {
    true
}

/// What a save/test tells the user about whether the provider actually works.
#[derive(Debug, Serialize)]
struct ProviderProbe {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    models: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SaveProviderResponse {
    settings: SettingsResponse,
    probe: ProviderProbe,
}

/// `POST /api/providers`: persist the provider (and its key), then probe it.
///
/// The provider is saved even when the probe fails — a typo'd key should
/// leave an editable row, not vanish — and the probe result says so plainly.
async fn save_provider(
    State(state): State<Arc<GuiState>>,
    axum::Json(body): axum::Json<SaveProvider>,
) -> Result<axum::Json<SaveProviderResponse>, ApiError> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(ApiError::bad_request("the provider needs a name"));
    }
    let kind: ProviderKind = parse_kind(&body.kind)?;
    let base_url = body.base_url.trim().to_string();
    let model = body.model.trim().to_string();
    if base_url.is_empty() {
        return Err(ApiError::bad_request("the provider needs a base URL"));
    }
    if model.is_empty() {
        return Err(ApiError::bad_request("the provider needs a model"));
    }
    if let Some(key) = &body.api_key {
        settings::store_key(&name, key)?;
    }

    let provider = ProviderConfig {
        name: name.clone(),
        kind,
        base_url,
        model,
        // The key lives in the credential file under this provider's name;
        // an env var would be a second source of truth for the same secret.
        api_key_env: None,
        gguf_path: None,
        usd_per_mtok_in: None,
        usd_per_mtok_out: None,
    };
    let config = state.config.update({
        let provider = provider.clone();
        move |config| {
            settings::upsert_provider(config, provider, body.activate);
            Ok(())
        }
    })?;

    Ok(axum::Json(SaveProviderResponse {
        probe: probe(&provider).await,
        settings: settings_response(&config),
    }))
}

/// `POST /api/providers/{name}/test`: does this saved provider answer?
async fn test_provider(
    Path(name): Path<String>,
    State(state): State<Arc<GuiState>>,
) -> Result<axum::Json<ProviderProbe>, ApiError> {
    let config = state.config.current();
    let provider = config
        .providers
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| ApiError::not_found(format!("no provider named '{name}'")))?;
    Ok(axum::Json(probe(provider).await))
}

/// `POST /api/providers/{name}/active`: switch the active provider.
async fn activate_provider(
    Path(name): Path<String>,
    State(state): State<Arc<GuiState>>,
) -> Result<axum::Json<SettingsResponse>, ApiError> {
    let config = state.config.update(|config| {
        anyhow::ensure!(
            config.providers.iter().any(|p| p.name == name),
            "no provider named '{name}'"
        );
        config.active_provider = Some(name.clone());
        Ok(())
    })?;
    Ok(axum::Json(settings_response(&config)))
}

/// `DELETE /api/providers/{name}`: forget the provider and its stored key.
async fn delete_provider(
    Path(name): Path<String>,
    State(state): State<Arc<GuiState>>,
) -> Result<axum::Json<SettingsResponse>, ApiError> {
    let config = state
        .config
        .update(|config| settings::remove_provider(config, &name))?;
    // Best-effort: a leftover key is harmless, but leaving it behind would
    // silently reattach to a provider re-added under the same name later.
    if let Err(err) = crate::credentials::remove(&name) {
        tracing::warn!("could not remove the stored key for '{name}': {err:#}");
    }
    Ok(axum::Json(settings_response(&config)))
}

/// Build the provider's client and ask it for its models: the cheapest call
/// that proves the base URL, the key, and the network all work.
async fn probe(provider: &ProviderConfig) -> ProviderProbe {
    let client = match provider.build() {
        Ok(client) => client,
        Err(err) => {
            return ProviderProbe {
                ok: false,
                error: Some(format!("{err:#}")),
                models: Vec::new(),
            };
        }
    };
    match tokio::time::timeout(PROBE_TIMEOUT, client.list_models()).await {
        Ok(Ok(models)) => ProviderProbe {
            ok: true,
            error: None,
            models,
        },
        Ok(Err(err)) => ProviderProbe {
            ok: false,
            error: Some(format!("{err:#}")),
            models: Vec::new(),
        },
        Err(_) => ProviderProbe {
            ok: false,
            error: Some(format!(
                "the provider did not answer within {}s",
                PROBE_TIMEOUT.as_secs()
            )),
            models: Vec::new(),
        },
    }
}

fn parse_kind(kind: &str) -> Result<ProviderKind, ApiError> {
    #[derive(Deserialize)]
    struct Probe {
        kind: ProviderKind,
    }
    toml::from_str::<Probe>(&format!("kind = {kind:?}"))
        .map(|probe| probe.kind)
        .map_err(|_| ApiError::bad_request(format!("unknown provider kind '{kind}'")))
}

/* ---------------------------------------------------------------------- */
/* Slash commands                                                         */
/* ---------------------------------------------------------------------- */

/// One row of `GET /api/commands`.
///
/// `where` says who executes it:
/// - `server` — sent back as a `command` frame and applied to the Agent
///   ([`crate::gui::tasks::apply_command`]).
/// - `client` — the page's own (a panel, an overlay, a list): nothing to ask the
///   server for.
/// - `unavailable` — terminal-only ([`Execution::Terminal`]). Offered so the menu
///   can say what it is and why it is not here, rather than pretend it never
///   existed; invoking it anyway is answered with an honest `error` frame.
/// - `prompt` — a custom `.wizard/commands/*.md` command, which the client sends
///   as an ordinary `user_message` and the *server* expands through
///   [`crate::commands::preprocess`], exactly as the TUI does.
///
/// The built-ins are derived from [`crate::commands::COMMANDS`] — the one table
/// the TUI completes from — so the two surfaces cannot drift into offering
/// different commands.
#[derive(Debug, Serialize)]
struct CommandRow {
    name: String,
    detail: String,
    #[serde(rename = "where")]
    executed_by: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<&'static str>,
}

impl From<&'static CommandSpec> for CommandRow {
    fn from(spec: &'static CommandSpec) -> Self {
        Self {
            name: spec.name.to_string(),
            detail: spec.description.to_string(),
            executed_by: spec.gui.wire(),
            args: (!spec.args.is_empty()).then_some(spec.args),
        }
    }
}

/// `/help`, as the `notice` frame that answers it: everything this surface runs,
/// straight off the shared table, plus an honest line about what it does not.
pub(crate) fn help_text() -> String {
    let mut text = String::from("commands:");
    for spec in crate::commands::COMMANDS
        .iter()
        .filter(|spec| spec.gui != Execution::Terminal)
    {
        match spec.args.is_empty() {
            true => text.push_str(&format!("\n  /{} — {}", spec.name, spec.description)),
            false => text.push_str(&format!(
                "\n  /{} {} — {}",
                spec.name, spec.args, spec.description
            )),
        }
    }
    let terminal: Vec<String> = crate::commands::commands_where(Execution::Terminal)
        .map(|spec| format!("/{}", spec.name))
        .collect();
    if !terminal.is_empty() {
        text.push_str(&format!("\n\nterminal only: {}", terminal.join(", ")));
    }
    text.push_str("\n\nplus any custom command in .wizard/commands/*.md, and @path to");
    text.push_str(" reference a file.");
    text
}

/// `GET /api/commands?cwd=/abs/path`: the built-ins plus the custom commands
/// loaded for that workspace, so the composer's menu is the one the *server*
/// will actually honor.
async fn commands(Query(query): Query<GitQuery>) -> Result<axum::Json<Value>, ApiError> {
    let root = PathBuf::from(&query.cwd);
    if !root.is_dir() {
        return Err(ApiError::bad_request(format!(
            "cwd '{}' is not a directory",
            query.cwd
        )));
    }
    let mut rows: Vec<CommandRow> = crate::commands::COMMANDS
        .iter()
        .map(CommandRow::from)
        .collect();
    for command in crate::commands::load(&root) {
        let args = command.expects_args().then_some("<args>");
        let detail = command.description.unwrap_or_else(|| {
            format!(
                "custom command ({})",
                command
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
            )
        });
        rows.push(CommandRow {
            name: command.name,
            detail,
            executed_by: "prompt",
            args,
        });
    }
    Ok(axum::Json(json!({ "commands": rows })))
}

/* ---------------------------------------------------------------------- */
/* Attachments                                                            */
/* ---------------------------------------------------------------------- */

/// One file `POST /api/tasks/{id}/upload` took in.
#[derive(Debug, Serialize)]
struct Attachment {
    path: String,
    name: String,
    mime: String,
    bytes: usize,
    /// `image` or `file` — decided from the *bytes*, never from what the client
    /// called it.
    kind: &'static str,
}

/// `POST /api/tasks/{id}/upload`: take in one or more `file` parts of a
/// `multipart/form-data` body and hand back the paths a `user_message` may
/// attach.
///
/// Task-scoped because both stores are session-scoped: an image has to land in
/// *this* session's image directory, since [`resolve_image`] serves nothing
/// from anywhere else.
///
/// The media type is sniffed from the bytes ([`crate::llm::sniff_mime`]). An
/// image goes through [`crate::llm::Image::from_bytes`] — which enforces the
/// size cap — into the content-addressed [`crate::images::ImageStore`]; anything
/// else is written to `~/.wizard/attachments/<session>/` under a sanitized name.
/// A client that labels a PDF `image/png`, or names it `x.png`, changes nothing:
/// neither the label nor the extension is read.
async fn upload(
    Path(id): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Result<axum::Json<Value>, ApiError> {
    let session = session_id(&id)?;
    let mut attachments = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| ApiError::bad_request(format!("reading the upload: {err}")))?
    {
        let name = field.file_name().unwrap_or_default().to_string();
        let bytes = field
            .bytes()
            .await
            .map_err(|err| ApiError::bad_request(format!("reading the upload: {err}")))?;
        attachments.push(store_attachment(session, &name, &bytes)?);
    }
    if attachments.is_empty() {
        return Err(ApiError::bad_request("the upload carried no files"));
    }
    Ok(axum::Json(json!({ "attachments": attachments })))
}

/// Write one uploaded file to the store its own bytes put it in.
fn store_attachment(session: &str, name: &str, bytes: &[u8]) -> Result<Attachment, ApiError> {
    let name = sanitize_name(name);
    if let Some(mime) = crate::llm::sniff_mime(bytes) {
        // The cap lives in `Image::from_bytes`; the store then names the file
        // after the hash of these exact bytes, which is what makes the path it
        // returns safe to hand back and cache forever.
        let image = crate::llm::Image::from_bytes(bytes)
            .map_err(|err| ApiError::bad_request(format!("{name}: {err}")))?;
        let saved = crate::images::ImageStore::open(session)
            .and_then(|store| store.save(&image))
            .map_err(|err| ApiError::from(err.context(format!("saving {name}"))))?;
        return Ok(Attachment {
            path: saved.path.display().to_string(),
            name,
            mime: mime.to_string(),
            bytes: saved.bytes,
            kind: "image",
        });
    }
    if bytes.len() > crate::llm::MAX_IMAGE_BYTES {
        return Err(ApiError::bad_request(format!(
            "{name} is {} bytes, over the {} byte cap",
            bytes.len(),
            crate::llm::MAX_IMAGE_BYTES
        )));
    }
    let dir = Config::attachments_dir()?.join(session);
    std::fs::create_dir_all(&dir)
        .map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err}")))?;
    let path = dir.join(&name);
    std::fs::write(&path, bytes)
        .map_err(|err| ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err}")))?;
    Ok(Attachment {
        path: path.display().to_string(),
        name,
        // Cosmetic only — a label for the chip in the composer. Nothing is
        // decided by it: `kind` above came from the bytes, and the turn reads
        // the file through the same `@file` expansion the TUI uses.
        mime: "application/octet-stream".to_string(),
        bytes: bytes.len(),
        kind: "file",
    })
}

/// A session id fit to name a directory: the store paths are built from it, so
/// a `..` or a `/` in it would be a write outside the store. Session ids are
/// timestamps (`2026-07-11T09-12-33`); anything with other characters in it is
/// not one.
fn session_id(id: &str) -> Result<&str, ApiError> {
    let ok = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !ok {
        return Err(ApiError::bad_request(format!("'{id}' is not a task id")));
    }
    Ok(id)
}

/// An uploaded file's name, reduced to something safe to join onto a directory:
/// the basename only, with anything that is not a plain name character folded to
/// `_`. Whitespace included — an attachment is referenced as an `@/abs/path`
/// token in the prompt, and a space in the path would split the token in half.
fn sanitize_name(name: &str) -> String {
    let base = FsPath::new(name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `..`, `.`, and the empty name are all directory traversal or nonsense.
    let cleaned = cleaned.trim_start_matches('.').to_string();
    if cleaned.is_empty() {
        return "attachment".to_string();
    }
    cleaned
}

/* ---------------------------------------------------------------------- */
/* Images                                                                 */
/* ---------------------------------------------------------------------- */

/// The extensions [`crate::images::ImageStore`] writes
/// ([`crate::llm::Image::extension`]) — and so the only ones [`image`] serves.
const IMAGE_EXTENSIONS: [&str; 4] = ["png", "jpg", "webp", "gif"];

/// `GET /api/image?path=...` query.
#[derive(Debug, Deserialize)]
struct ImageQuery {
    path: String,
}

/// `GET /api/image`: the bytes of one image the agent wrote, named by the path
/// an `images` frame (or a replayed `images` item) carried.
///
/// `path` is client input, so it is resolved against `~/.wizard/images/` and
/// nothing else ([`resolve_image`]): this route hands out image files, not any
/// file the page cares to name.
async fn image(Query(query): Query<ImageQuery>) -> Result<Response, ApiError> {
    let path = resolve_image(&Config::images_dir()?, &query.path)?;
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|err| ApiError::not_found(format!("{}: {err}", query.path)))?;
    // The media type comes from the file's own magic number, not from anything
    // the client said and not from the extension: it is the one answer that
    // cannot be talked into mislabelling the bytes the browser is about to run
    // through an image decoder. It doubles as the last content check — a file
    // in the store that is not an image is not served.
    let mime = crate::llm::sniff_mime(&bytes)
        .ok_or_else(|| ApiError::bad_request(format!("{} is not an image", query.path)))?;
    Ok((
        [
            (header::CONTENT_TYPE, mime),
            // The file name is the hash of these exact bytes, so the URL can
            // never come to mean anything else: cache it for good.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        bytes,
    )
        .into_response())
}

/// Resolve a client-supplied image path against the image store, or refuse it.
///
/// The rules, in order: the name must end in an image extension; the path must
/// resolve — `..` segments, symlinks and all — to a *regular file* really
/// inside `root` ([`resolve_in_store`]). So `../../../etc/passwd`, an absolute
/// path elsewhere on the disk, and a symlink in the store pointing out of it
/// all land in the same refusal.
fn resolve_image(root: &FsPath, path: &str) -> Result<PathBuf, ApiError> {
    let refused = || ApiError::bad_request(format!("'{path}' is not an image wizard saved"));
    let extension = FsPath::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    if !extension.is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension.as_str())) {
        return Err(refused());
    }
    resolve_in_store(root, path, refused)
}

/// Resolve a client-supplied attachment path against the attachment store, or
/// refuse it — [`resolve_image`] without the image-extension rule, since an
/// attachment is any file the user uploaded.
fn resolve_attachment(root: &FsPath, path: &str) -> Result<PathBuf, ApiError> {
    resolve_in_store(root, path, || {
        ApiError::bad_request(format!("'{path}' is not a file wizard saved"))
    })
}

/// The containment check both stores share: the path must canonicalize — `..`
/// segments, symlinks and all, which is what [`std::fs::canonicalize`] does —
/// to a regular file inside `root`. A file that is simply gone is a 404 the
/// page can render honestly; anything else is `refused`.
///
/// This is the *only* place a client-supplied absolute path is admitted. Every
/// route that takes one goes through it: a second implementation is a second
/// chance to get it wrong.
fn resolve_in_store(
    root: &FsPath,
    path: &str,
    refused: impl Fn() -> ApiError,
) -> Result<PathBuf, ApiError> {
    // Both sides are canonicalized before they are compared: a symlinked root
    // (a home directory that is one, a store under /tmp on macOS) would
    // otherwise fail to prefix-match its own files.
    let root = root.canonicalize().map_err(|_| refused())?;
    let file = FsPath::new(path)
        .canonicalize()
        .map_err(|err| ApiError::not_found(format!("{path}: {err}")))?;
    if !file.starts_with(&root) || !file.is_file() {
        return Err(refused());
    }
    Ok(file)
}

/// Verify the attachment paths on a `user_message` frame: images must sit in
/// `~/.wizard/images/`, files in `~/.wizard/attachments/`. Nothing else is a
/// path this server put in the client's hands.
///
/// Called on every message, not just the ones the upload route answered: a
/// socket can send whatever it likes, and the turn is about to read these files
/// into the model's context.
pub(crate) fn verify_attachments(
    images: &[String],
    files: &[String],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let message = |err: ApiError| err.1;
    let images_root = Config::images_dir().map_err(|err| format!("{err:#}"))?;
    let files_root = Config::attachments_dir().map_err(|err| format!("{err:#}"))?;
    let images = images
        .iter()
        .map(|path| resolve_image(&images_root, path).map_err(message))
        .collect::<Result<Vec<_>, _>>()?;
    let files = files
        .iter()
        .map(|path| resolve_attachment(&files_root, path).map_err(message))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((images, files))
}

/// `GET /api/git?cwd=...` query.
#[derive(Debug, Deserialize)]
struct GitQuery {
    cwd: String,
}

async fn git_status(Query(query): Query<GitQuery>) -> Result<axum::Json<git::GitStatus>, ApiError> {
    let root = PathBuf::from(&query.cwd);
    if !root.is_dir() {
        return Err(ApiError::bad_request(format!(
            "cwd '{}' is not a directory",
            query.cwd
        )));
    }
    let status = git::status(&root)
        .await
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    Ok(axum::Json(status))
}

/// `GET /api/git/diff?cwd=...&path=...` query.
#[derive(Debug, Deserialize)]
struct GitDiffQuery {
    cwd: String,
    path: String,
}

/// `GET /api/git/diff`: one changed file's diff against HEAD (staged and
/// unstaged together, matching the file's `+N -M` in `GET /api/git`).
///
/// `path` is client input, and [`git::diff`] takes only paths `GET /api/git`
/// itself just listed for this workspace — anything else is a 400 rather than
/// an argument handed to git.
async fn git_diff(
    Query(query): Query<GitDiffQuery>,
) -> Result<axum::Json<git::FileDiff>, ApiError> {
    let root = PathBuf::from(&query.cwd);
    if !root.is_dir() {
        return Err(ApiError::bad_request(format!(
            "cwd '{}' is not a directory",
            query.cwd
        )));
    }
    let diff = git::diff(&root, &query.path)
        .await
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    Ok(axum::Json(diff))
}

/// `GET /api/git/branches?cwd=...`: local branches of the workspace.
async fn git_branches(
    Query(query): Query<GitQuery>,
) -> Result<axum::Json<git::Branches>, ApiError> {
    let root = PathBuf::from(&query.cwd);
    if !root.is_dir() {
        return Err(ApiError::bad_request(format!(
            "cwd '{}' is not a directory",
            query.cwd
        )));
    }
    let branches = git::branches(&root)
        .await
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    Ok(axum::Json(branches))
}

/// `POST /api/git/checkout` body.
#[derive(Debug, Deserialize)]
struct CheckoutBody {
    cwd: String,
    branch: String,
    /// Create the branch from the current HEAD (`git checkout -b`).
    #[serde(default)]
    create: bool,
    /// The chat whose workspace this is, when the switch comes from one. A
    /// running turn is mid-edit in this working tree, so its branch is not
    /// something to change under it.
    #[serde(default)]
    task: Option<String>,
}

async fn git_checkout(
    State(state): State<Arc<GuiState>>,
    axum::Json(body): axum::Json<CheckoutBody>,
) -> Result<axum::Json<Value>, ApiError> {
    let root = PathBuf::from(&body.cwd);
    if !root.is_dir() {
        return Err(ApiError::bad_request(format!(
            "cwd '{}' is not a directory",
            body.cwd
        )));
    }
    if let Some(id) = &body.task
        && let Some(task) = state.manager.get(id)
        && task.state() == TaskState::Working
    {
        return Err(ApiError::bad_request(
            "the agent is working in this branch — stop the turn first",
        ));
    }
    let branch = git::checkout(&root, &body.branch, body.create)
        .await
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    Ok(axum::Json(json!({ "ok": true, "branch": branch })))
}

/// One row of `GET /api/workspaces`: a directory the GUI can open a chat in.
#[derive(Debug, Serialize)]
struct WorkspaceRow {
    cwd: String,
    name: String,
    task_count: usize,
    /// The directory `wizard gui` itself runs in.
    home: bool,
}

/// `GET /api/workspaces`: the directories of every known chat (sessions on
/// disk + the live registry) plus the server's own, busiest first — what the
/// topbar's folder chip offers.
async fn workspaces(
    State(state): State<Arc<GuiState>>,
) -> Result<axum::Json<Vec<WorkspaceRow>>, ApiError> {
    let sessions_dir = Config::sessions_dir()?;
    let mut by_cwd: HashMap<String, HashSet<String>> = HashMap::new();
    for summary in session::summaries(&sessions_dir) {
        if let Some(cwd) = summary.cwd {
            by_cwd.entry(cwd).or_default().insert(summary.id);
        }
    }
    for record in session_registry::list() {
        by_cwd.entry(record.cwd).or_default().insert(record.id);
    }
    let home = state.cwd.display().to_string();
    by_cwd.entry(home.clone()).or_default();
    let mut rows: Vec<WorkspaceRow> = by_cwd
        .into_iter()
        // A directory that has since been deleted or renamed cannot host a
        // chat, and offering it would only produce a 400 on click.
        .filter(|(cwd, _)| FsPath::new(cwd).is_dir())
        .map(|(cwd, ids)| WorkspaceRow {
            name: basename(&cwd),
            home: cwd == home,
            cwd,
            task_count: ids.len(),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.home
            .cmp(&a.home)
            .then(b.task_count.cmp(&a.task_count))
            .then(a.cwd.cmp(&b.cwd))
    });
    Ok(axum::Json(rows))
}

/// The `/api/tasks` state string for a registry record.
fn registry_state(state: SessionState) -> &'static str {
    match state {
        SessionState::Working => "working",
        SessionState::NeedsInput => "needs_input",
        SessionState::Idle => "idle",
        SessionState::Completed => "done",
        SessionState::Failed => "failed",
    }
}

/// A directory's display name: its basename, or the path itself when it has
/// none (e.g. `/`).
fn basename(cwd: &str) -> String {
    FsPath::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.to_string())
}

/// A file's mtime as unix seconds.
fn mtime_unix(path: &FsPath) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The menu the page shows is the table the TUI completes from. Two lists
    /// would be two behaviors: a command the GUI advertises and cannot run, or
    /// one it runs and never offers — which is how `/goal` came to be missing
    /// from a GUI whose agent could already set the mission.
    #[test]
    fn the_commands_the_gui_offers_are_the_ones_the_table_defines() {
        let rows: Vec<CommandRow> = crate::commands::COMMANDS
            .iter()
            .map(CommandRow::from)
            .collect();
        assert_eq!(
            rows.len(),
            crate::commands::COMMANDS.len(),
            "every built-in has a row — including the ones this surface refuses, \
             which say so rather than vanishing"
        );

        let row = |name: &str| {
            rows.iter()
                .find(|row| row.name == name)
                .unwrap_or_else(|| panic!("/{name} is offered"))
        };
        assert_eq!(row("goal").executed_by, "server");
        assert_eq!(row("goal").args, Some("[text]"));
        assert_eq!(row("diff").executed_by, "client");
        assert_eq!(row("vim").executed_by, "unavailable");

        // The detail text is the table's own: one description per command, for
        // every surface that names it.
        assert_eq!(
            row("goal").detail,
            crate::commands::spec("goal").unwrap().description
        );
    }

    /// `/help` answers with what this surface actually runs, and is honest about
    /// what it does not.
    #[test]
    fn help_lists_the_runnable_commands_and_names_the_terminal_only_ones() {
        let text = help_text();
        assert!(
            text.contains("/goal [text] — show the standing goal, or set one and start working")
        );
        assert!(text.contains("/diff"));
        assert!(
            !text.contains("\n  /vim"),
            "not offered as a command: {text}"
        );
        assert!(
            text.contains("terminal only: /vim, /quit, /exit"),
            "but named, not silently absent: {text}"
        );
        assert!(text.contains(".wizard/commands/*.md"));
    }

    #[test]
    fn basenames_fall_back_to_the_path_itself() {
        assert_eq!(basename("/home/user/projects/wizard"), "wizard");
        assert_eq!(basename("/"), "/");
        assert_eq!(basename(""), "");
    }

    #[test]
    fn registry_states_map_to_protocol_strings() {
        assert_eq!(registry_state(SessionState::Working), "working");
        assert_eq!(registry_state(SessionState::NeedsInput), "needs_input");
        assert_eq!(registry_state(SessionState::Idle), "idle");
        assert_eq!(registry_state(SessionState::Completed), "done");
        assert_eq!(registry_state(SessionState::Failed), "failed");
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                header::HeaderName::try_from(*name).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn images_resolve_only_inside_the_store() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("images").join("2026-07-13T09-00-00");
        std::fs::create_dir_all(&root).unwrap();
        let png = root.join("c414cd0e204de974.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        let store = home.path().join("images");

        // The path an `images` frame carried.
        let resolved = resolve_image(&store, &png.display().to_string()).expect("served");
        assert_eq!(resolved, png.canonicalize().unwrap());

        // A secret outside the store, reached three ways.
        let secret = home.path().join("credentials.toml");
        std::fs::write(&secret, "key = 'sk-1'").unwrap();
        let traversal = root.join("../../credentials.toml");
        for path in [
            secret.display().to_string(),
            traversal.display().to_string(),
        ] {
            let refusal = resolve_image(&store, &path).expect_err("refused");
            assert_eq!(refusal.0, StatusCode::BAD_REQUEST, "{path}");
        }
        // Renaming the traversal to look like an image does not make it one:
        // the extension passes, the resolved path is still outside the store.
        let disguised = root.join("../../credentials.toml.png");
        std::fs::write(home.path().join("credentials.toml.png"), "key = 'sk-1'").unwrap();
        assert!(resolve_image(&store, &disguised.display().to_string()).is_err());

        // A symlink inside the store pointing out of it resolves to its target,
        // which is outside — the same refusal, not a hole.
        #[cfg(unix)]
        {
            let escape = root.join("escape.png");
            std::os::unix::fs::symlink(&secret, &escape).unwrap();
            assert!(resolve_image(&store, &escape.display().to_string()).is_err());
        }

        // Not an image name at all, and a directory that ends in one.
        assert!(resolve_image(&store, &root.join("notes.txt").display().to_string()).is_err());
        let dir = root.join("nested.png");
        std::fs::create_dir(&dir).unwrap();
        assert!(resolve_image(&store, &dir.display().to_string()).is_err());

        // A file that is simply gone (the session was cleaned up) is a 404, so
        // the page can say so rather than sit on a blank box.
        let missing = resolve_image(&store, &root.join("deadbeef.png").display().to_string())
            .expect_err("missing");
        assert_eq!(missing.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn attachments_resolve_only_inside_their_own_store() {
        let home = tempfile::tempdir().unwrap();
        let store = home.path().join("attachments");
        let root = store.join("2026-07-13T09-00-00");
        std::fs::create_dir_all(&root).unwrap();
        let spec = root.join("spec.pdf");
        std::fs::write(&spec, b"%PDF-1.7").unwrap();

        let resolved = resolve_attachment(&store, &spec.display().to_string()).expect("served");
        assert_eq!(resolved, spec.canonicalize().unwrap());

        // The same refusals `resolve_image` gives — this is that guard, reused.
        let secret = home.path().join("credentials.toml");
        std::fs::write(&secret, "key = 'sk-1'").unwrap();
        let traversal = root.join("../../credentials.toml");
        for path in [
            secret.display().to_string(),
            traversal.display().to_string(),
        ] {
            let refusal = resolve_attachment(&store, &path).expect_err("refused");
            assert_eq!(refusal.0, StatusCode::BAD_REQUEST, "{path}");
        }
        #[cfg(unix)]
        {
            let escape = root.join("escape.pdf");
            std::os::unix::fs::symlink(&secret, &escape).unwrap();
            assert!(resolve_attachment(&store, &escape.display().to_string()).is_err());
        }
        // An image path is not an attachment path: each store answers for its
        // own, so an uploaded image cannot be laundered into a file ref.
        let images = home.path().join("images");
        std::fs::create_dir_all(&images).unwrap();
        let png = images.join("ab12.png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G']).unwrap();
        assert!(resolve_attachment(&store, &png.display().to_string()).is_err());
        assert!(resolve_image(&images, &spec.display().to_string()).is_err());
    }

    #[test]
    fn an_upload_is_classified_by_its_bytes_not_by_what_the_client_called_it() {
        let session = "2026-07-13T10-00-00";
        // A PNG the client swore was a PDF, and a PDF it swore was a PNG. The
        // filename and the content-type are both ignored; only the magic number
        // decides where the file lands.
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let image = store_attachment(session, "not-an-image.pdf", &png).expect("stored");
        assert_eq!(image.kind, "image");
        assert_eq!(image.mime, "image/png");
        // In the image store, under a name that is the hash of its bytes — the
        // only place `GET /api/image` will serve it back from.
        let images_root = Config::images_dir().unwrap();
        assert!(
            FsPath::new(&image.path).starts_with(&images_root),
            "{}",
            image.path
        );
        assert!(image.path.ends_with(".png"), "{}", image.path);
        assert!(resolve_image(&images_root, &image.path).is_ok());

        let file =
            store_attachment(session, "screenshot.png", b"%PDF-1.7\nnot a png").expect("stored");
        assert_eq!(file.kind, "file", "the bytes are not an image");
        let attachments_root = Config::attachments_dir().unwrap();
        assert!(
            FsPath::new(&file.path).starts_with(&attachments_root),
            "a non-image never enters the image store: {}",
            file.path
        );
        assert_eq!(file.name, "screenshot.png");
        // And it is not servable as an image, whatever it is called.
        assert!(resolve_image(&Config::images_dir().unwrap(), &file.path).is_err());
    }

    #[test]
    fn uploaded_names_cannot_escape_the_session_directory() {
        // A name is a name, not a path: traversal, separators, and the
        // whitespace that would split the `@/abs/path` token in the prompt are
        // all folded away.
        assert_eq!(sanitize_name("../../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("/etc/shadow"), "shadow");
        assert_eq!(sanitize_name("my notes.txt"), "my_notes.txt");
        assert_eq!(sanitize_name(".."), "attachment");
        assert_eq!(sanitize_name(""), "attachment");
        assert_eq!(sanitize_name(".ssh"), "ssh");
        assert_eq!(sanitize_name("report-v2.final.pdf"), "report-v2.final.pdf");

        // The session id names a directory, so it is checked the same way.
        assert!(session_id("2026-07-13T09-00-00").is_ok());
        assert!(session_id("../../etc").is_err());
        assert!(session_id("a/b").is_err());
        assert!(session_id(".").is_err());
        assert!(session_id("").is_err());
    }

    #[test]
    fn local_requests_pass_the_guard() {
        // Plain HTTP tools and same-origin navigations carry no Origin.
        assert!(is_local_request(&headers(&[("host", "127.0.0.1:4680")])));
        assert!(is_local_request(&headers(&[("host", "localhost:4680")])));
        assert!(is_local_request(&headers(&[("host", "localhost")])));
        assert!(is_local_request(&headers(&[("host", "[::1]:4680")])));
        // Cross-origin fetches and WS upgrades from the app's own pages.
        assert!(is_local_request(&headers(&[
            ("host", "127.0.0.1:4680"),
            ("origin", "http://127.0.0.1:4680"),
        ])));
        assert!(is_local_request(&headers(&[
            ("host", "localhost:4680"),
            ("origin", "http://localhost:4680"),
        ])));
    }

    #[test]
    fn drive_by_requests_are_rejected() {
        // DNS rebinding: a hostile name resolving to 127.0.0.1.
        assert!(!is_local_request(&headers(&[(
            "host",
            "evil.example:4680"
        )])));
        assert!(!is_local_request(&headers(&[(
            "host",
            "localhost.evil.example"
        )])));
        assert!(!is_local_request(&headers(&[])));
        // A foreign page opening the API or the WS upgrade.
        assert!(!is_local_request(&headers(&[
            ("host", "127.0.0.1:4680"),
            ("origin", "https://evil.example"),
        ])));
        assert!(!is_local_request(&headers(&[
            ("host", "127.0.0.1:4680"),
            ("origin", "null"),
        ])));
        // An Origin that merely mentions localhost is not local.
        assert!(!is_local_request(&headers(&[
            ("host", "127.0.0.1:4680"),
            ("origin", "http://localhost.evil.example"),
        ])));
        assert!(!host_is_local("[::1"));
        assert!(!host_is_local("[::1]x"));
    }

    /* ------------------------------------------------------------------ */
    /* The served router: real sockets on 127.0.0.1:0, so the guard, the  */
    /* extractors and the error mapping run exactly as a browser hits them */
    /* ------------------------------------------------------------------ */

    use std::net::SocketAddr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    use crate::gui::settings::ConfigStore;
    use crate::gui::tasks::TaskManager;

    /// State over a fresh store, heartbeating nowhere (a test run must not
    /// advertise itself as a live session).
    fn state_in(cwd: &FsPath) -> Arc<GuiState> {
        let store = Arc::new(ConfigStore::new(Config::default()));
        Arc::new(GuiState {
            manager: TaskManager::with_registry(
                Arc::clone(&store),
                Arc::new(tokio::sync::RwLock::new(crate::mcp::McpManager::empty())),
                None,
            ),
            config: store,
            cwd: cwd.to_path_buf(),
            assets_dir: None,
            sign_in: Arc::new(oauth::SignIn::default()),
        })
    }

    async fn serve(state: Arc<GuiState>) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("bind the test server");
        let addr = listener.local_addr().expect("the bound address");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router(state)).await;
        });
        addr
    }

    /// One HTTP/1.1 exchange: status, lowercased header block, body.
    async fn send_bytes(addr: SocketAddr, request: Vec<u8>) -> (u16, String, String) {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        stream.write_all(&request).await.expect("write the request");
        read_response(&mut stream).await
    }

    async fn read_response(stream: &mut TcpStream) -> (u16, String, String) {
        let mut raw = Vec::new();
        let mut chunk = [0u8; 4096];
        let split = loop {
            if let Some(pos) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                break pos;
            }
            let n = stream.read(&mut chunk).await.expect("read");
            assert!(n > 0, "the server closed before finishing the headers");
            raw.extend_from_slice(&chunk[..n]);
        };
        let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|len| len.trim().parse().ok())
            .unwrap_or(0);
        let mut body = raw[split + 4..].to_vec();
        while body.len() < length {
            let n = stream.read(&mut chunk).await.expect("read");
            assert!(n > 0, "the server closed mid-body");
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(length);
        (status, head, String::from_utf8_lossy(&body).into_owned())
    }

    async fn get(addr: SocketAddr, path: &str) -> (u16, String, String) {
        send_bytes(
            addr,
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
                .into_bytes(),
        )
        .await
    }

    async fn with_body(
        addr: SocketAddr,
        method: &str,
        path: &str,
        body: &str,
    ) -> (u16, String, String) {
        send_bytes(
            addr,
            format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
                 Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .into_bytes(),
        )
        .await
    }

    fn parsed(body: &str) -> Value {
        serde_json::from_str(body).unwrap_or_else(|err| panic!("{err}: {body}"))
    }

    #[tokio::test]
    async fn the_guard_fronts_every_route_it_serves() {
        let cwd = tempfile::tempdir().unwrap();
        let addr = serve(state_in(cwd.path())).await;

        // A rebound Host is refused on the page, the API, and the WS upgrade
        // path alike: the predicate is unit-tested above, this is the proof the
        // middleware actually fronts the routes.
        for path in ["/", "/api/workspace", "/api/tasks/x/ws"] {
            let (status, _, _) = send_bytes(
                addr,
                format!("GET {path} HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n")
                    .into_bytes(),
            )
            .await;
            assert_eq!(status, 403, "{path} answered a rebound Host");
        }
        let (status, _, _) = send_bytes(
            addr,
            "GET /api/workspace HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Origin: https://evil.example\r\nConnection: close\r\n\r\n"
                .into(),
        )
        .await;
        assert_eq!(status, 403, "a foreign Origin is refused on a local Host");

        let (status, _, body) = get(addr, "/api/workspace").await;
        assert_eq!(status, 200);
        assert_eq!(parsed(&body)["cwd"], cwd.path().display().to_string());
    }

    #[tokio::test]
    async fn code_assets_are_served_uncached_and_fonts_cached_hard() {
        let cwd = tempfile::tempdir().unwrap();
        let addr = serve(state_in(cwd.path())).await;

        let (status, head, body) = get(addr, "/").await;
        assert_eq!(status, 200);
        assert!(head.contains("content-type: text/html"), "{head}");
        assert!(head.contains("cache-control: no-store"), "{head}");
        assert!(!body.is_empty());

        let (status, head, _) = get(addr, "/app.js").await;
        assert_eq!(status, 200);
        assert!(head.contains("content-type: text/javascript"), "{head}");
        assert!(head.contains("cache-control: no-store"), "{head}");

        let (status, head, _) = get(addr, "/fonts/inter.woff2").await;
        assert_eq!(status, 200);
        assert!(head.contains("content-type: font/woff2"), "{head}");
        assert!(head.contains("immutable"), "a font never changes: {head}");

        let (status, _, body) = get(addr, "/fonts/comic-sans.woff2").await;
        assert_eq!(status, 404);
        assert!(body.contains("no font"), "{body}");

        let (status, head, _) = get(addr, "/favicon.ico").await;
        assert_eq!(status, 200);
        assert!(head.contains("image/svg"), "{head}");
    }

    #[tokio::test]
    async fn creating_a_task_validates_the_cwd_and_lists_it_back() {
        let cwd = tempfile::tempdir().unwrap();
        let addr = serve(state_in(cwd.path())).await;

        for bad in ["relative/path", "/definitely/not/a/dir"] {
            let (status, _, body) = with_body(
                addr,
                "POST",
                "/api/tasks",
                &format!(r#"{{ "cwd": "{bad}" }}"#),
            )
            .await;
            assert_eq!(status, 400, "{bad}");
            assert!(body.contains("error"), "{body}");
        }

        let (status, _, _) = with_body(addr, "POST", "/api/tasks", "{ not json").await;
        assert_eq!(status, 400, "malformed JSON is refused, not defaulted");

        // No cwd in the body: the chat opens where the server was launched.
        let (status, _, body) = with_body(addr, "POST", "/api/tasks", "{}").await;
        assert_eq!(status, 201);
        let created = parsed(&body);
        assert_eq!(created["cwd"], cwd.path().display().to_string());
        let workspace = cwd.path().file_name().unwrap().to_string_lossy();
        assert_eq!(created["workspace"], workspace.as_ref());
        let id = created["id"].as_str().expect("an id").to_string();
        assert!(!id.is_empty());

        // An empty chat has nothing for the sidebar; a first message makes it a
        // session on disk, merged with this manager's live state.
        Session::open_by_id(&Config::sessions_dir().unwrap(), &id)
            .unwrap()
            .expect("the session file exists")
            .append(&crate::llm::ChatMessage::user("hello gui"))
            .unwrap();
        let (status, _, body) = get(addr, "/api/tasks").await;
        assert_eq!(status, 200);
        let row = parsed(&body)
            .as_array()
            .expect("a listing")
            .iter()
            .find(|row| row["id"] == id.as_str())
            .cloned()
            .unwrap_or_else(|| panic!("the new task is listed: {body}"));
        assert_eq!(row["state"], "idle", "live state, not the on-disk default");
        assert_eq!(row["title"], "hello gui");
        assert_eq!(row["workspace"], workspace.as_ref());

        let (status, _, body) = get(addr, &format!("/api/tasks/{id}")).await;
        assert_eq!(status, 200);
        let detail = parsed(&body);
        assert_eq!(detail["cwd"], cwd.path().display().to_string());
        let items = detail["items"].as_array().expect("items");
        assert_eq!(items.len(), 1, "the replayed transcript: {body}");
        assert_eq!(items[0]["kind"], "user");
        assert_eq!(items[0]["text"], "hello gui");

        let (status, _, _) = get(addr, "/api/tasks/2020-01-01T00-00-00").await;
        assert_eq!(status, 404, "a session that does not exist is a 404");
    }

    /* --- the WebSocket, over a real upgrade --- */

    async fn ws_upgrade(addr: SocketAddr, id: &str) -> (u16, String, TcpStream) {
        let mut stream = TcpStream::connect(addr).await.expect("connect");
        let request = format!(
            "GET /api/tasks/{id}/ws HTTP/1.1\r\nHost: 127.0.0.1\r\n\
             Connection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.expect("write");
        // One byte at a time: the first WS frame can coalesce with the 101
        // header block, and an over-read here would swallow it.
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            stream
                .read_exact(&mut byte)
                .await
                .expect("closed during the handshake");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
        let status: u16 = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        (status, head, stream)
    }

    async fn read_ws_text(stream: &mut TcpStream) -> String {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.expect("frame header");
        assert_eq!(header[0], 0x81, "one unfragmented text frame");
        let mut length = u64::from(header[1] & 0x7f);
        if length == 126 {
            let mut extended = [0u8; 2];
            stream.read_exact(&mut extended).await.expect("length");
            length = u64::from(u16::from_be_bytes(extended));
        }
        let mut payload = vec![0u8; length as usize];
        stream.read_exact(&mut payload).await.expect("payload");
        String::from_utf8(payload).expect("text")
    }

    async fn send_ws_text(stream: &mut TcpStream, text: &str) {
        let mask = [7u8, 13, 42, 9];
        let mut frame = vec![0x81];
        match text.len() {
            len if len < 126 => frame.push(0x80 | len as u8),
            len => {
                frame.push(0x80 | 126);
                frame.extend((len as u16).to_be_bytes());
            }
        }
        frame.extend(mask);
        frame.extend(text.bytes().zip(mask.iter().cycle()).map(|(b, m)| b ^ m));
        stream.write_all(&frame).await.expect("send frame");
    }

    #[tokio::test]
    async fn the_task_socket_upgrades_snapshots_state_and_answers_bad_frames() {
        let cwd = tempfile::tempdir().unwrap();
        let state = state_in(cwd.path());
        let id = state
            .manager
            .create_task(cwd.path(), None, None)
            .expect("an empty chat");
        let addr = serve(state).await;

        let (status, _, _) = ws_upgrade(addr, "2020-01-01T00-00-00").await;
        assert_eq!(status, 404, "an unknown id is refused before the upgrade");

        let (status, head, mut socket) = ws_upgrade(addr, &id).await;
        assert_eq!(status, 101);
        assert!(head.contains("sec-websocket-accept"), "{head}");

        let frame = parsed(&read_ws_text(&mut socket).await);
        assert_eq!(frame["type"], "state", "attach opens with the snapshot");
        assert_eq!(frame["state"], "idle");

        // A frame that is not the protocol's is answered on this socket, not
        // dropped — the page cannot fix what it never hears about.
        send_ws_text(&mut socket, "certainly not json").await;
        let frame = parsed(&read_ws_text(&mut socket).await);
        assert_eq!(frame["type"], "error");
        assert!(
            frame["message"]
                .as_str()
                .unwrap()
                .contains("unrecognized frame"),
            "{frame}"
        );

        // A verdict with no plan pending is a protocol error, not a hang.
        send_ws_text(
            &mut socket,
            r#"{ "type": "plan_verdict", "approve": true }"#,
        )
        .await;
        let frame = parsed(&read_ws_text(&mut socket).await);
        assert_eq!(frame["type"], "error");
        assert!(
            frame["message"].as_str().unwrap().contains("no plan"),
            "{frame}"
        );
    }

    /// The one test that writes providers and credentials: the config file and
    /// the credential store are process-shared in tests, so the whole lifecycle
    /// runs serially here.
    #[tokio::test]
    async fn the_settings_routes_edit_the_config_and_probe_saved_providers_honestly() {
        let cwd = tempfile::tempdir().unwrap();
        let addr = serve(state_in(cwd.path())).await;

        let (status, _, body) =
            with_body(addr, "PATCH", "/api/settings", r#"{ "max_steps": 1001 }"#).await;
        assert_eq!(status, 400);
        assert!(body.contains("at most 1000"), "{body}");

        let (status, _, body) =
            with_body(addr, "PATCH", "/api/settings", r#"{ "max_steps": 250 }"#).await;
        assert_eq!(status, 200);
        assert_eq!(parsed(&body)["max_steps"], 250);

        let (status, _, body) = with_body(
            addr,
            "POST",
            "/api/providers",
            r#"{ "name": " ", "kind": "openai", "base_url": "http://127.0.0.1:9", "model": "m" }"#,
        )
        .await;
        assert_eq!(status, 400);
        assert!(body.contains("needs a name"), "{body}");

        let (status, _, body) = with_body(
            addr,
            "POST",
            "/api/providers",
            r#"{ "name": "p", "kind": "warpdrive", "base_url": "http://127.0.0.1:9", "model": "m" }"#,
        )
        .await;
        assert_eq!(status, 400);
        assert!(body.contains("unknown provider kind"), "{body}");

        // A backend that refuses the connection still saves — a typo'd endpoint
        // must leave an editable row — and the probe says plainly that it failed.
        // Port 9 (discard) on loopback refuses instantly; nothing leaves the box.
        let (status, _, body) = with_body(
            addr,
            "POST",
            "/api/providers",
            r#"{ "name": "route-stub", "kind": "openai", "base_url": "http://127.0.0.1:9",
                 "model": "m", "api_key": "sk-route-test" }"#,
        )
        .await;
        assert_eq!(status, 200);
        let saved = parsed(&body);
        assert_eq!(saved["probe"]["ok"], false);
        assert!(saved["probe"]["error"].as_str().is_some(), "{saved}");
        assert_eq!(saved["settings"]["first_run"], false);
        assert_eq!(saved["settings"]["active"], "route-stub");
        assert_eq!(saved["settings"]["providers"][0]["name"], "route-stub");
        assert_eq!(saved["settings"]["providers"][0]["key"], "stored");
        assert_eq!(
            crate::credentials::get("route-stub").as_deref(),
            Some("sk-route-test")
        );

        // Editing without retyping the key keeps the stored one.
        let (status, _, body) = with_body(
            addr,
            "POST",
            "/api/providers",
            r#"{ "name": "route-stub", "kind": "openai", "base_url": "http://127.0.0.1:9",
                 "model": "m2" }"#,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(parsed(&body)["settings"]["providers"][0]["key"], "stored");
        assert_eq!(
            crate::credentials::get("route-stub").as_deref(),
            Some("sk-route-test")
        );

        let (status, _, body) = with_body(addr, "POST", "/api/providers/route-stub/test", "").await;
        assert_eq!(status, 200);
        assert_eq!(parsed(&body)["ok"], false);

        let (status, _, _) = with_body(addr, "POST", "/api/providers/ghost/test", "").await;
        assert_eq!(status, 404, "an unknown provider is not probed");

        // Deleting forgets the provider and its stored key with it.
        let (status, _, body) = with_body(addr, "DELETE", "/api/providers/route-stub", "").await;
        assert_eq!(status, 200);
        let settings = parsed(&body);
        assert_eq!(settings["providers"].as_array().unwrap().len(), 0);
        assert_eq!(settings["first_run"], true);
        assert_eq!(crate::credentials::get("route-stub"), None);
    }

    #[tokio::test]
    async fn the_commands_route_merges_the_workspace_customs() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(cwd.path().join(".wizard/commands")).unwrap();
        std::fs::write(
            cwd.path().join(".wizard/commands/ship.md"),
            "Ship $ARGUMENTS carefully",
        )
        .unwrap();
        let addr = serve(state_in(cwd.path())).await;

        let (status, _, _) = get(addr, "/api/commands?cwd=/not/a/dir").await;
        assert_eq!(status, 400);

        let (status, _, body) =
            get(addr, &format!("/api/commands?cwd={}", cwd.path().display())).await;
        assert_eq!(status, 200);
        let listing = parsed(&body);
        let rows = listing["commands"].as_array().expect("rows");
        let row = |name: &str| {
            rows.iter()
                .find(|row| row["name"] == name)
                .unwrap_or_else(|| panic!("/{name} is offered: {body}"))
        };
        assert_eq!(row("goal")["where"], "server");
        assert_eq!(row("ship")["where"], "prompt");
        assert_eq!(row("ship")["args"], "<args>");
    }

    const BOUNDARY: &str = "wizard-test-boundary";

    async fn post_upload(
        addr: SocketAddr,
        id: &str,
        parts: &[(&str, &[u8])],
    ) -> (u16, String, String) {
        let mut body: Vec<u8> = Vec::new();
        for (name, bytes) in parts {
            body.extend_from_slice(
                format!(
                    "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; \
                     filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            );
            body.extend_from_slice(bytes);
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
        let mut request = format!(
            "POST /api/tasks/{id}/upload HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\
             Content-Type: multipart/form-data; boundary={BOUNDARY}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        request.extend_from_slice(&body);
        send_bytes(addr, request).await
    }

    #[tokio::test]
    async fn the_upload_route_checks_the_id_shape_and_lands_files_by_their_bytes() {
        let cwd = tempfile::tempdir().unwrap();
        let addr = serve(state_in(cwd.path())).await;
        let png = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

        // `%2E%2E` decodes to `..`: an id that is a traversal, not a session.
        let (status, _, body) = post_upload(addr, "%2E%2E", &[("a.png", &png)]).await;
        assert_eq!(status, 400);
        assert!(body.contains("not a task id"), "{body}");

        let (status, _, body) = post_upload(addr, "2026-07-17T00-00-01", &[]).await;
        assert_eq!(status, 400);
        assert!(body.contains("no files"), "{body}");

        // The name says PDF; the bytes say PNG. The bytes win, end to end.
        let (status, _, body) =
            post_upload(addr, "2026-07-17T00-00-01", &[("shot.pdf", &png)]).await;
        assert_eq!(status, 200);
        let upload = parsed(&body);
        assert_eq!(upload["attachments"][0]["kind"], "image");
        assert_eq!(upload["attachments"][0]["mime"], "image/png");
    }

    #[tokio::test]
    async fn client_paths_that_name_no_workspace_or_store_are_400s() {
        let cwd = tempfile::tempdir().unwrap();
        let addr = serve(state_in(cwd.path())).await;

        for path in [
            "/api/git?cwd=/not/a/dir",
            "/api/git/branches?cwd=/not/a/dir",
            "/api/git/diff?cwd=/not/a/dir&path=a.txt",
        ] {
            let (status, _, body) = get(addr, path).await;
            assert_eq!(status, 400, "{path}");
            assert!(body.contains("not a directory"), "{path}: {body}");
        }
        let (status, _, _) = with_body(
            addr,
            "POST",
            "/api/git/checkout",
            r#"{ "cwd": "/not/a/dir", "branch": "main" }"#,
        )
        .await;
        assert_eq!(status, 400);

        let (status, _, _) = get(addr, "/api/image?path=/etc/passwd").await;
        assert_eq!(status, 400, "a path outside the store is refused");
        let (status, _, _) = get(addr, "/api/image").await;
        assert_eq!(status, 400, "a missing query is a bad request");
    }
}
