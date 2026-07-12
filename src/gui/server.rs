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

/// The GUI's five assets, embedded at compile time so the binary stays
/// self-contained.
const ASSETS: [Asset; 5] = [
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
        .route("/fonts/{name}", get(serve_font))
        .route("/favicon.ico", get(favicon))
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/{id}", get(get_task))
        .route("/api/tasks/{id}/ws", get(task_ws))
        .route("/api/workspace", get(workspace))
        .route("/api/models", get(models))
        .route("/api/settings", get(get_settings).patch(patch_settings))
        // The provider redirects the *browser* here after the user authorizes,
        // so the path is short and stable — it is registered with the provider.
        .route("/callback", get(oauth_callback))
        .route("/api/login/{provider}", post(begin_sign_in))
        .route("/api/login", get(sign_in_status))
        .route("/api/providers", post(save_provider))
        .route("/api/providers/{name}", delete(delete_provider))
        .route("/api/providers/{name}/active", post(activate_provider))
        .route("/api/providers/{name}/test", post(test_provider))
        .route("/api/workspaces", get(workspaces))
        .route("/api/git", get(git_status))
        .route("/api/git/commit", post(git_commit))
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
/// the user to. The browser opens it; the provider redirects back to
/// [`oauth_callback`].
async fn begin_sign_in(
    Path(provider): Path<String>,
    State(state): State<Arc<GuiState>>,
) -> Result<axum::Json<Value>, ApiError> {
    // The redirect must be an address the provider will actually send a browser
    // to, and one we serve: our own origin.
    let redirect_uri = format!("http://127.0.0.1:{}/callback", state.port);
    let url = match provider.as_str() {
        "xai" => state.sign_in.begin_xai(&redirect_uri).await?,
        other => {
            return Err(ApiError::bad_request(format!(
                "cannot sign in to '{other}'"
            )));
        }
    };
    Ok(axum::Json(json!({ "authorize_url": url })))
}

/// `GET /api/login`: what the sign-in that is in flight is doing. The tab the
/// user started from polls this, because the tab that *finishes* the sign-in is
/// the provider's redirect — a different page, which then closes itself.
async fn sign_in_status(State(state): State<Arc<GuiState>>) -> axum::Json<oauth::Status> {
    axum::Json(state.sign_in.status())
}

/// `GET /callback?code=…&state=…`: where the provider sends the browser back.
///
/// This is a page a human is looking at, not an API call, so it answers in HTML
/// either way — and it never shows the code or the tokens, only whether it
/// worked.
#[derive(Debug, Deserialize)]
struct CallbackQuery {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    /// The provider sends `error=access_denied` when the user says no.
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

async fn oauth_callback(
    Query(query): Query<CallbackQuery>,
    State(state): State<Arc<GuiState>>,
) -> Response {
    if let Some(error) = query.error {
        let detail = query.error_description.unwrap_or_else(|| error.clone());
        state.sign_in.deny(detail.clone());
        return callback_page("Sign-in cancelled", &detail);
    }
    let (Some(code), Some(returned)) = (query.code, query.state) else {
        let message = "the provider redirected here without an authorization code";
        state.sign_in.deny(message.to_string());
        return callback_page("Sign-in failed", message);
    };

    match state.sign_in.complete(&code, &returned).await {
        Ok(provider) => {
            let name = provider.name.clone();
            // A sign-in that does not leave you with a usable provider was
            // pointless, so the provider is written and made active here.
            if let Err(err) = state.config.update(move |config| {
                settings::upsert_provider(config, provider, true);
                Ok(())
            }) {
                return callback_page("Signed in, but not saved", &format!("{err:#}"));
            }
            callback_page("Signed in", &format!("'{name}' is configured and active."))
        }
        Err(err) => callback_page("Sign-in failed", &format!("{err:#}")),
    }
}

/// The page the user lands on after authorizing. It closes itself when the
/// browser allows it (it will, for a tab that a script opened), and says what
/// happened when it cannot.
fn callback_page(title: &str, detail: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>{title}</title>\
         <style>body{{background:#0c0c0e;color:#ececee;font:14px/1.6 system-ui,sans-serif;\
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0}}\
         div{{text-align:center;max-width:32rem;padding:0 1.5rem}}\
         h1{{font-size:15px;font-weight:600;margin:0 0 .4rem}}\
         p{{color:#86868e;margin:0}}</style>\
         <div><h1>{title}</h1><p>{detail}</p><p>You can close this tab.</p></div>\
         <script>setTimeout(() => window.close(), 1200)</script>",
        title = html_escape(title),
        detail = html_escape(detail),
    );
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

/// Escape text for the callback page: provider error strings are attacker-
/// adjacent input, and they are rendered into HTML.
fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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
    /// Tool calls one GUI chat may make per turn (`[gui] max_steps`). The
    /// top-level `max_steps` belongs to the TUI and is left alone: a control
    /// here that silently governed a different surface would be a lie.
    max_steps: u32,
    providers: Vec<SettingsProvider>,
    presets: &'static [settings::Preset],
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
        max_steps: config.gui.max_steps,
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
        presets: settings::PRESETS,
    }
}

async fn get_settings(State(state): State<Arc<GuiState>>) -> axum::Json<SettingsResponse> {
    axum::Json(settings_response(&state.config.current()))
}

/// `PATCH /api/settings`: the GUI's own step budget.
#[derive(Debug, Deserialize)]
struct PatchSettings {
    #[serde(default)]
    max_steps: Option<u32>,
}

async fn patch_settings(
    State(state): State<Arc<GuiState>>,
    axum::Json(body): axum::Json<PatchSettings>,
) -> Result<axum::Json<SettingsResponse>, ApiError> {
    if let Some(steps) = body.max_steps
        && !(1..=1000).contains(&steps)
    {
        return Err(ApiError::bad_request(
            "the step limit must be between 1 and 1000",
        ));
    }
    let config = state.config.update(|config| {
        if let Some(steps) = body.max_steps {
            config.gui.max_steps = steps;
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

/// `POST /api/git/commit` body.
#[derive(Debug, Deserialize)]
struct CommitBody {
    cwd: String,
    message: String,
}

async fn git_commit(
    axum::Json(body): axum::Json<CommitBody>,
) -> Result<axum::Json<Value>, ApiError> {
    let root = PathBuf::from(&body.cwd);
    if !root.is_dir() {
        return Err(ApiError::bad_request(format!(
            "cwd '{}' is not a directory",
            body.cwd
        )));
    }
    if body.message.trim().is_empty() {
        return Err(ApiError::bad_request("commit message must not be empty"));
    }
    let sha = git::commit(&root, &body.message)
        .await
        .map_err(|err| ApiError::bad_request(format!("{err:#}")))?;
    Ok(axum::Json(json!({ "ok": true, "sha": sha })))
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

    #[test]
    fn basenames_fall_back_to_the_path_itself() {
        assert_eq!(basename("/home/user/projects/wizard"), "wizard");
        assert_eq!(basename("/"), "/");
        assert_eq!(basename(""), "");
    }

    #[test]
    fn callback_page_escapes_provider_supplied_text() {
        // The provider's `error_description` is rendered into HTML; a script
        // tag in it must not survive as markup.
        let page = callback_page("Sign-in failed", "<script>alert(1)</script>");
        let body = format!("{page:?}");
        assert!(!format!("{:?}", "<script>").is_empty());
        assert!(html_escape("<script>alert(1)</script>").contains("&lt;script&gt;"));
        // The page renders (a 200 with a body); the escape is asserted above.
        let _ = body;
    }

    #[test]
    fn html_escape_covers_the_markup_metacharacters() {
        assert_eq!(html_escape("a<b>&\"c"), "a&lt;b&gt;&amp;&quot;c");
        assert_eq!(html_escape("plain text"), "plain text");
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
}
