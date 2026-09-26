//! SSH devices: machines added in Settings → Devices (`{data_dir}/ssh-devices.json`)
//! that this engine reaches through an `ssh -L` tunnel to their own engine's IPC
//! port, using the user's own ssh keys, agent, and config.
//!
//! A connected peer contributes its device, space, chat, and session rows to this
//! engine's watch streams, so the one window lists and drives it like a local
//! device. Calls that belong to it are relayed over the tunnel: anything
//! addressed by `targetDeviceId`, anything naming a chat or space it owns, and
//! Mutates on its registry rows (see [`SshPeers::route`]). Its rows stay in its
//! own registry; nothing of the peer is written here.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use zeron_proto::{Chat, Device, Session, Space};
use zeron_rpc::{RpcClient, methods};

/// `{data_dir}/ssh-devices.json`: the hosts added in Settings → Devices.
pub const FILE_NAME: &str = "ssh-devices.json";
/// `{data_dir}/ssh-peers.json`: host → device id learned on connect, so a
/// peer's calls route to it (and fail clearly) even before it reconnects.
const KNOWN_FILE: &str = "ssh-peers.json";
/// The engine IPC port on the remote (what every install listens on).
pub const REMOTE_IPC_PORT: u16 = 27654;

/// Options shared by every non-interactive ssh call: never prompt, fail fast,
/// and notice a dead link instead of hanging.
pub const SSH_OPTIONS: &[&str] = &[
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
];

/// A connected peer's device row is re-stamped this often, so presence
/// (a 70s window) reads it as online for as long as the tunnel is up.
const PRESENCE_TICK: Duration = Duration::from_secs(20);
const TUNNEL_READY_TIMEOUT: Duration = Duration::from_secs(20);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// An ssh destination we are willing to pass as an argument: an alias or
/// `user@host[:port]`-ish token, never something ssh would read as an option.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-@:[]%".contains(c))
}

/// Start (or with `restart`, replace) the remote engine as a systemd user
/// service when possible, else in the background, and wait until its IPC port
/// answers. Run with `sh -s` on the remote.
pub fn remote_start_script(restart: bool) -> String {
    format!(
        r#"
export PATH="$HOME/.local/bin:$PATH"
PORT={port}
RESTART={restart}
probe() {{ (bash -c "</dev/tcp/127.0.0.1/$PORT") >/dev/null 2>&1 || nc -z 127.0.0.1 "$PORT" >/dev/null 2>&1; }}
Z="$HOME/.local/bin/zeron"
[ -x "$Z" ] || Z="$(command -v zeron 2>/dev/null || true)"
if [ -z "$Z" ]; then echo "the Wizard GUI engine isn't installed on this machine — run setup again" >&2; exit 3; fi
if [ "$RESTART" = 0 ] && probe; then echo engine=running; exit 0; fi
started=""
if [ "$(uname -s)" = Linux ] && command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
  if "$Z" daemon install >/dev/null 2>&1; then
    systemctl --user restart zeron.service >/dev/null 2>&1 && started=systemd
    loginctl enable-linger "$(id -un)" >/dev/null 2>&1 || true
  fi
fi
if [ -z "$started" ]; then
  if [ "$RESTART" = 1 ]; then pkill -f 'zeron headless' >/dev/null 2>&1 || true; sleep 1; fi
  if ! probe; then
    if command -v setsid >/dev/null 2>&1; then
      (setsid nohup "$Z" headless >/dev/null 2>&1 </dev/null &)
    else
      (nohup "$Z" headless >/dev/null 2>&1 </dev/null &)
    fi
  fi
  started=background
fi
i=0
while [ $i -lt 60 ]; do
  if probe; then echo "engine=$started"; exit 0; fi
  sleep 0.5
  i=$((i+1))
done
echo "the engine didn't start listening on 127.0.0.1:$PORT" >&2
exit 5
"#,
        port = REMOTE_IPC_PORT,
        restart = u8::from(restart),
    )
}

#[derive(Deserialize)]
struct SshDeviceEntry {
    host: String,
}

/// The hosts in `{data_dir}/ssh-devices.json`, in file order; empty when the
/// file is missing or unreadable.
pub fn configured_hosts(data_dir: &Path) -> Vec<String> {
    let entries: Vec<SshDeviceEntry> = std::fs::read_to_string(data_dir.join(FILE_NAME))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let mut hosts: Vec<String> = Vec::new();
    for entry in entries {
        if valid_host(&entry.host) && !hosts.contains(&entry.host) {
            hosts.push(entry.host);
        }
    }
    hosts
}

/// One host's last known state and rows (rows survive a dropped tunnel so the
/// sidebar keeps its chats; presence then lapses and they read as offline).
#[derive(Debug, Clone, Default)]
pub struct PeerView {
    pub device_id: Option<String>,
    pub connected: bool,
    pub error: Option<String>,
    devices: Vec<Device>,
    spaces: Vec<Space>,
    chats: Vec<Chat>,
    sessions: Vec<Session>,
}

#[derive(Debug, Clone, Default)]
pub struct PeersState {
    pub peers: BTreeMap<String, PeerView>,
}

impl PeersState {
    fn owner_of(&self, device_id: &str) -> Option<&PeerView> {
        self.peers
            .values()
            .find(|peer| peer.device_id.as_deref() == Some(device_id))
    }

    fn devices(&self, now: chrono::DateTime<Utc>) -> Vec<Device> {
        self.peers
            .values()
            .flat_map(|peer| {
                peer.devices.iter().cloned().map(move |mut device| {
                    if peer.connected {
                        device.last_seen_at = Some(now);
                    }
                    device
                })
            })
            .collect()
    }

    fn spaces(&self) -> Vec<Space> {
        self.peers.values().flat_map(|p| p.spaces.clone()).collect()
    }

    fn chats(&self) -> Vec<Chat> {
        self.peers.values().flat_map(|p| p.chats.clone()).collect()
    }

    fn sessions(&self) -> Vec<Session> {
        self.peers
            .values()
            .flat_map(|p| p.sessions.clone())
            .collect()
    }
}

struct Inner {
    data_dir: PathBuf,
    local_device_id: String,
    /// Host → device id, persisted in [`KNOWN_FILE`].
    known: Mutex<HashMap<String, String>>,
    state: watch::Sender<PeersState>,
    /// Live connection per peer device id.
    clients: Mutex<HashMap<String, Arc<RpcClient>>>,
    /// Running host tasks and their stop tokens.
    tasks: Mutex<HashMap<String, CancellationToken>>,
    /// Chats minted on a peer before its chat rows arrive here — so the send
    /// that follows a createChat reaches the same machine.
    routes: Mutex<HashMap<String, String>>,
    shutdown: CancellationToken,
}

/// The engine's SSH peers; cheap to clone.
#[derive(Clone)]
pub struct SshPeers {
    inner: Arc<Inner>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl SshPeers {
    /// Connect to every configured host (must run on a tokio runtime).
    pub fn start(data_dir: PathBuf, local_device_id: String) -> Self {
        let (state, _) = watch::channel(PeersState::default());
        let peers = Self {
            inner: Arc::new(Inner {
                known: Mutex::new(load_known(&data_dir)),
                data_dir,
                local_device_id,
                state,
                clients: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
                routes: Mutex::new(HashMap::new()),
                shutdown: CancellationToken::new(),
            }),
        };
        peers.reload();
        peers
    }

    /// Re-read `ssh-devices.json`: connect new hosts, drop removed ones.
    pub fn reload(&self) {
        if self.inner.shutdown.is_cancelled() {
            return;
        }
        let hosts = configured_hosts(&self.inner.data_dir);
        let mut tasks = lock(&self.inner.tasks);
        let removed: Vec<String> = tasks
            .keys()
            .filter(|host| !hosts.contains(host))
            .cloned()
            .collect();
        for host in removed {
            if let Some(stop) = tasks.remove(&host) {
                stop.cancel();
            }
            self.inner.state.send_modify(|state| {
                if let Some(peer) = state.peers.remove(&host)
                    && let Some(device) = peer.device_id
                {
                    lock(&self.inner.clients).remove(&device);
                }
            });
        }
        for host in hosts {
            if tasks.contains_key(&host) {
                continue;
            }
            let stop = self.inner.shutdown.child_token();
            tasks.insert(host.clone(), stop.clone());
            let known = lock(&self.inner.known).get(&host).cloned();
            self.inner.state.send_modify(|state| {
                let peer = state.peers.entry(host.clone()).or_default();
                if peer.device_id.is_none() {
                    peer.device_id = known;
                }
            });
            tokio::spawn(run_host(self.inner.clone(), host, stop));
        }
    }

    /// Serve `host` over an already-connected engine client instead of an
    /// ssh tunnel (in-process tests and embedders). Its rows and routes behave
    /// exactly as a tunnelled peer's until `stop` is cancelled.
    pub fn attach(&self, host: &str, client: Arc<RpcClient>) -> CancellationToken {
        let stop = self.inner.shutdown.child_token();
        self.inner.state.send_modify(|state| {
            state.peers.entry(host.to_string()).or_default();
        });
        let (inner, host, task_stop) = (self.inner.clone(), host.to_string(), stop.clone());
        tokio::spawn(async move {
            let _ = serve(&inner, &host, client, &task_stop, std::future::pending()).await;
            inner.state.send_modify(|state| {
                if let Some(peer) = state.peers.get_mut(&host) {
                    peer.connected = false;
                    if let Some(device) = &peer.device_id {
                        lock(&inner.clients).remove(device);
                    }
                }
            });
        });
        stop
    }

    /// Close every tunnel (engine shutdown).
    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
        lock(&self.inner.tasks).clear();
        lock(&self.inner.clients).clear();
    }

    pub fn watch(&self) -> watch::Receiver<PeersState> {
        self.inner.state.subscribe()
    }

    /// Whether any SSH host is configured.
    pub fn any(&self) -> bool {
        !self.inner.state.borrow().peers.is_empty()
    }

    /// Chat-scoped watches the UI opens the moment it selects a chat — for a
    /// brand-new chat, before the createChat that says which machine hosts
    /// it. Opening those locally would bind them to an empty local doc for
    /// good, so they wait for the owner instead ([`Self::await_chat_owner`]).
    pub fn waits_for_owner(method: &str) -> bool {
        matches!(method, methods::WATCH_DOC_MESSAGES | methods::WATCH_QUEUE)
    }

    /// The peer hosting `chat_id`, waiting up to `deadline` while nothing
    /// claims it yet. `None` when `known_locally` says this engine has it, or
    /// when nobody does in time (the local doc host then materializes it, as
    /// before SSH peers existed).
    pub async fn await_chat_owner(
        &self,
        chat_id: &str,
        known_locally: impl Fn(&str) -> bool,
        deadline: Duration,
    ) -> Option<String> {
        let parent = chat_id.split("--sub--").next().unwrap_or(chat_id);
        let mut state = self.inner.state.subscribe();
        let give_up = tokio::time::Instant::now() + deadline;
        loop {
            if let Some(owner) = self.chat_owner(chat_id) {
                return Some(owner);
            }
            if known_locally(parent) {
                return None;
            }
            // Routes from createChat are not signalled; poll alongside the
            // peers' row updates.
            tokio::select! {
                _ = state.changed() => {}
                _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                _ = tokio::time::sleep_until(give_up) => return None,
            }
        }
    }

    /// Whether `device_id` is one of the SSH peers (connected or not).
    pub fn is_peer_device(&self, device_id: &str) -> bool {
        device_id != self.inner.local_device_id
            && self.inner.state.borrow().owner_of(device_id).is_some()
    }

    fn peer_name(&self, device_id: &str) -> String {
        let state = self.inner.state.borrow();
        state
            .peers
            .iter()
            .find(|(_, peer)| peer.device_id.as_deref() == Some(device_id))
            .map(|(host, _)| host.clone())
            .unwrap_or_else(|| device_id.to_string())
    }

    /// The live connection to a peer, or why there isn't one.
    pub fn client(&self, device_id: &str) -> Result<Arc<RpcClient>, zeron_rpc::RpcError> {
        lock(&self.inner.clients)
            .get(device_id)
            .cloned()
            .ok_or_else(|| {
                zeron_rpc::RpcError::Transport(format!(
                    "{} isn't connected right now (its ssh tunnel is down; it reconnects on its own)",
                    self.peer_name(device_id)
                ))
            })
    }

    /// The peer device hosting `chat_id` (a subagent doc resolves to its chat).
    pub fn chat_owner(&self, chat_id: &str) -> Option<String> {
        let chat_id = chat_id.split("--sub--").next().unwrap_or(chat_id);
        if let Some(device) = lock(&self.inner.routes).get(chat_id) {
            return Some(device.clone());
        }
        let state = self.inner.state.borrow();
        state.peers.values().find_map(|peer| {
            peer.chats
                .iter()
                .find(|chat| chat.id == chat_id)
                .map(|chat| chat.device_id.clone())
        })
    }

    /// The peer device owning `space_id`.
    pub fn space_owner(&self, space_id: &str) -> Option<String> {
        let state = self.inner.state.borrow();
        state.peers.values().find_map(|peer| {
            peer.spaces
                .iter()
                .find(|space| space.id == space_id)
                .map(|space| space.device_id.clone())
        })
    }

    /// Which peer, if any, a call belongs to:
    /// - an explicit `targetDeviceId` naming a peer (for `forwardable` methods);
    /// - a Mutate on a peer's chat, space, or device row, or one minting a
    ///   chat on a peer's space or device;
    /// - otherwise a `chatId` (then `spaceId`) the peer owns.
    pub fn route(&self, method: &str, params: &Value, forwardable: bool) -> Option<String> {
        let text = |key: &str| params.get(key).and_then(Value::as_str);
        if let Some(target) = text("targetDeviceId") {
            return (forwardable && self.is_peer_device(target)).then(|| target.to_string());
        }
        if method == methods::MUTATE {
            return self.mutate_owner(params);
        }
        if let Some(chat_id) = text("chatId") {
            return self.chat_owner(chat_id);
        }
        text("spaceId").and_then(|space_id| self.space_owner(space_id))
    }

    fn mutate_owner(&self, params: &Value) -> Option<String> {
        let text = |key: &str| params.get(key).and_then(Value::as_str);
        let op = text("op")?;
        let owner = match op {
            "createChat" => {
                let owner = match text("spaceId") {
                    Some(space_id) => self.space_owner(space_id),
                    None => text("deviceId")
                        .filter(|device| self.is_peer_device(device))
                        .map(str::to_string),
                }?;
                if let Some(chat_id) = text("chatId") {
                    lock(&self.inner.routes).insert(chat_id.to_string(), owner.clone());
                }
                return Some(owner);
            }
            "createSpace" | "renameDevice" => text("deviceId")
                .filter(|device| self.is_peer_device(device))
                .map(str::to_string),
            "renameSpace" | "deleteSpace" => text("spaceId").and_then(|id| self.space_owner(id)),
            "changeSidebarPin" => None,
            _ => text("chatId").and_then(|id| self.chat_owner(id)),
        };
        owner
    }

    pub fn merged_devices(
        &self,
        local: watch::Receiver<Vec<Device>>,
    ) -> watch::Receiver<Vec<Device>> {
        self.merged(local, |state| state.devices(Utc::now()), |d| &d.id, true)
    }

    pub fn merged_spaces(&self, local: watch::Receiver<Vec<Space>>) -> watch::Receiver<Vec<Space>> {
        self.merged(local, PeersState::spaces, |s| &s.id, false)
    }

    pub fn merged_chats(&self, local: watch::Receiver<Vec<Chat>>) -> watch::Receiver<Vec<Chat>> {
        self.merged(local, PeersState::chats, |c| &c.id, false)
    }

    pub fn merged_sessions(
        &self,
        local: watch::Receiver<Vec<Session>>,
    ) -> watch::Receiver<Vec<Session>> {
        self.merged(local, PeersState::sessions, |s| &s.chat_id, false)
    }

    /// `local` with every peer's rows appended; a peer's row replaces a local
    /// row with the same id (it is the owner's copy). `presence` re-publishes
    /// on a timer so connected peers keep a fresh `last_seen_at`.
    fn merged<T>(
        &self,
        mut local: watch::Receiver<Vec<T>>,
        pick: fn(&PeersState) -> Vec<T>,
        id: fn(&T) -> &String,
        presence: bool,
    ) -> watch::Receiver<Vec<T>>
    where
        T: Clone + Send + Sync + 'static,
    {
        let mut peers = self.inner.state.subscribe();
        let combine = move |local: &[T], peers: &PeersState| merge_rows(local, pick(peers), id);
        let (tx, rx) = watch::channel(combine(&local.borrow(), &peers.borrow()));
        let shutdown = self.inner.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PRESENCE_TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tokio::select! {
                    changed = local.changed() => if changed.is_err() { break },
                    changed = peers.changed() => if changed.is_err() { break },
                    _ = tick.tick(), if presence => {}
                    _ = tx.closed() => break,
                    _ = shutdown.cancelled() => break,
                }
                let merged = combine(&local.borrow_and_update(), &peers.borrow_and_update());
                if tx.send(merged).is_err() {
                    break;
                }
            }
        });
        rx
    }
}

fn merge_rows<T: Clone>(local: &[T], peers: Vec<T>, id: fn(&T) -> &String) -> Vec<T> {
    if peers.is_empty() {
        return local.to_vec();
    }
    let mut merged: Vec<T> = local
        .iter()
        .filter(|row| !peers.iter().any(|peer| id(peer) == id(row)))
        .cloned()
        .collect();
    merged.extend(peers);
    merged
}

/// Keep one host connected until it is removed or the engine shuts down.
async fn run_host(inner: Arc<Inner>, host: String, stop: CancellationToken) {
    let mut backoff = BACKOFF_MIN;
    let mut started_engine = false;
    while !stop.is_cancelled() {
        let outcome = connect_once(&inner, &host, &stop).await;
        // Whatever ended the connection, its client is gone.
        let device = inner
            .state
            .borrow()
            .peers
            .get(&host)
            .and_then(|peer| peer.device_id.clone());
        if let Some(device) = &device {
            lock(&inner.clients).remove(device);
        }
        let error = match outcome {
            Ok(Outcome::WasUp) => {
                backoff = BACKOFF_MIN;
                started_engine = false;
                None
            }
            Ok(Outcome::EngineDown) if !started_engine => {
                // The tunnel works but nothing listens behind it: start the
                // remote engine once, then retry right away.
                started_engine = true;
                match start_remote_engine(&host).await {
                    Ok(()) => continue,
                    Err(err) => Some(err),
                }
            }
            Ok(Outcome::EngineDown) => {
                Some(format!("the Wizard GUI engine on {host} isn't answering"))
            }
            Err(err) => Some(err),
        };
        if let Some(error) = &error {
            tracing::warn!(%host, %error, "ssh peer unavailable");
        }
        inner.state.send_modify(|state| {
            if let Some(peer) = state.peers.get_mut(&host) {
                peer.connected = false;
                peer.error = error;
            }
        });
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.cancelled() => break,
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

enum Outcome {
    /// Connected and streaming until the link dropped.
    WasUp,
    /// The tunnel came up, but no engine answered behind it.
    EngineDown,
}

async fn connect_once(
    inner: &Arc<Inner>,
    host: &str,
    stop: &CancellationToken,
) -> Result<Outcome, String> {
    let port = free_local_port().map_err(|e| format!("no free local port: {e}"))?;
    let mut tunnel = spawn_tunnel(host, port).map_err(|e| format!("couldn't run ssh: {e}"))?;
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = tunnel.stderr.take() {
        let tail = stderr_tail.clone();
        let host = host.to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt as _;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(%host, "ssh: {line}");
                let mut tail = lock(&tail);
                tail.clear();
                tail.push_str(line.trim());
            }
        });
    }
    // Tunnel up: the local end of the forward accepts connections.
    let deadline = tokio::time::Instant::now() + TUNNEL_READY_TIMEOUT;
    loop {
        if let Ok(Some(status)) = tunnel.try_wait() {
            let detail = lock(&stderr_tail).clone();
            return Err(if detail.is_empty() {
                format!("ssh to {host} exited ({status})")
            } else {
                format!("ssh to {host} failed: {detail}")
            });
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline || stop.is_cancelled() {
            let _ = tunnel.kill().await;
            return Err(format!("the ssh tunnel to {host} didn't come up"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let client = match zeron_rpc::connect_ws(&format!("ws://127.0.0.1:{port}")).await {
        Ok(client) => Arc::new(client),
        Err(_) => {
            let _ = tunnel.kill().await;
            return Ok(Outcome::EngineDown);
        }
    };
    let outcome = serve(inner, host, client, stop, async {
        let _ = tunnel.wait().await;
    })
    .await;
    let _ = tunnel.kill().await;
    outcome
}

/// Speak to a connected peer engine until `link_down` fires, `stop` is
/// cancelled, or its registry streams end: learn its device id, mirror its
/// rows, and offer its client for relayed calls.
async fn serve(
    inner: &Arc<Inner>,
    host: &str,
    client: Arc<RpcClient>,
    stop: &CancellationToken,
    link_down: impl std::future::Future<Output = ()>,
) -> Result<Outcome, String> {
    let info = match tokio::time::timeout(
        Duration::from_secs(10),
        client.call(methods::ENGINE_INFO, serde_json::json!({})),
    )
    .await
    {
        Ok(Ok(info)) => info,
        _ => return Ok(Outcome::EngineDown),
    };
    let Some(device_id) = info
        .get("deviceId")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Err(format!("{host}'s engine didn't report a device id"));
    };
    if device_id == inner.local_device_id {
        return Err(format!("{host} is this device"));
    }
    let subscribe = |method: &'static str| {
        let client = client.clone();
        async move { client.subscribe(method, serde_json::json!({})).await }
    };
    let (devices, spaces, chats, sessions) = match tokio::try_join!(
        subscribe(methods::WATCH_DEVICES),
        subscribe(methods::WATCH_SPACES),
        subscribe(methods::WATCH_CHATS),
        subscribe(methods::WATCH_SESSIONS),
    ) {
        Ok(streams) => streams,
        Err(err) => {
            return Err(format!(
                "{host}'s engine refused its registry watches: {err}"
            ));
        }
    };
    let (mut devices, mut spaces, mut chats, mut sessions) = (devices, spaces, chats, sessions);
    lock(&inner.clients).insert(device_id.clone(), client.clone());
    remember(inner, host, &device_id);
    inner.state.send_modify(|state| {
        let peer = state.peers.entry(host.to_string()).or_default();
        peer.device_id = Some(device_id.clone());
        peer.connected = true;
        peer.error = None;
    });
    tracing::info!(%host, device = %device_id, "ssh peer connected");

    // Only the peer's own rows: its registry may also name devices it has
    // never been (a synced profile), which are not reachable through it.
    fn own<T: serde::de::DeserializeOwned>(
        value: Value,
        device_id: &str,
        of: fn(&T) -> &str,
    ) -> Option<Vec<T>> {
        let rows: Vec<T> = serde_json::from_value(value).ok()?;
        Some(
            rows.into_iter()
                .filter(|row| of(row) == device_id)
                .collect(),
        )
    }
    let update = |apply: &dyn Fn(&mut PeerView)| {
        inner.state.send_modify(|state| {
            if let Some(peer) = state.peers.get_mut(host) {
                apply(peer);
            }
        });
    };
    let link_down = std::pin::pin!(link_down);
    let mut link_down = link_down;
    loop {
        tokio::select! {
            item = devices.recv() => {
                let Some(item) = item else { break };
                if let Some(rows) = own::<Device>(item, &device_id, |d| &d.id) {
                    update(&|peer| peer.devices = rows.clone());
                }
            }
            item = spaces.recv() => {
                let Some(item) = item else { break };
                if let Some(rows) = own::<Space>(item, &device_id, |s| &s.device_id) {
                    update(&|peer| peer.spaces = rows.clone());
                }
            }
            item = chats.recv() => {
                let Some(item) = item else { break };
                if let Some(rows) = own::<Chat>(item, &device_id, |c| &c.device_id) {
                    // Rows the peer now publishes no longer need a pending route.
                    let mut routes = lock(&inner.routes);
                    for chat in &rows {
                        routes.remove(&chat.id);
                    }
                    drop(routes);
                    update(&|peer| peer.chats = rows.clone());
                }
            }
            item = sessions.recv() => {
                let Some(item) = item else { break };
                if let Some(rows) = own::<Session>(item, &device_id, |s| &s.device_id) {
                    update(&|peer| peer.sessions = rows.clone());
                }
            }
            _ = &mut link_down => break,
            _ = stop.cancelled() => break,
        }
    }
    tracing::info!(%host, "ssh peer disconnected");
    Ok(Outcome::WasUp)
}

fn load_known(data_dir: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(data_dir.join(KNOWN_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Record `host`'s device id (best effort; only writes on change).
fn remember(inner: &Inner, host: &str, device_id: &str) {
    let mut known = lock(&inner.known);
    if known.get(host).map(String::as_str) == Some(device_id) {
        return;
    }
    known.insert(host.to_string(), device_id.to_string());
    let Ok(text) = serde_json::to_vec_pretty(&*known) else {
        return;
    };
    let path = inner.data_dir.join(KNOWN_FILE);
    let staging = path.with_extension("json.tmp");
    if std::fs::write(&staging, text).is_ok() {
        let _ = std::fs::rename(&staging, &path);
    }
}

fn free_local_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?
        .local_addr()?
        .port())
}

fn spawn_tunnel(host: &str, port: u16) -> std::io::Result<tokio::process::Child> {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.arg("-N")
        .args(SSH_OPTIONS)
        .args(["-o", "ExitOnForwardFailure=yes"])
        .arg("-L")
        .arg(format!("{port}:127.0.0.1:{REMOTE_IPC_PORT}"))
        .arg(host)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    // The tunnel must not outlive the engine, even if it is killed.
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

async fn start_remote_engine(host: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;
    let mut child = tokio::process::Command::new("ssh")
        .args(SSH_OPTIONS)
        .arg(host)
        .args(["sh", "-s"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("couldn't run ssh: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(remote_start_script(false).as_bytes())
            .await
            .map_err(|e| format!("couldn't reach {host}: {e}"))?;
    }
    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| format!("starting the engine on {host} timed out"))?
        .map_err(|e| format!("couldn't reach {host}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if detail.is_empty() {
            format!("couldn't start the engine on {host}")
        } else {
            format!("couldn't start the engine on {host}: {detail}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str) -> Device {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": id, "platform": "linux", "lastSeenAt": null,
        }))
        .unwrap()
    }

    fn chat(id: &str, device_id: &str) -> Chat {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": device_id, "title": null, "archived": false,
            "cwd": null, "branch": null, "checkoutId": null,
            "createdAt": "2026-09-26T18:00:00Z",
        }))
        .unwrap()
    }

    fn space(id: &str, device_id: &str) -> Space {
        serde_json::from_value(serde_json::json!({
            "id": id, "deviceId": device_id, "path": "/home/me/project",
            "createdAt": "2026-09-26T18:00:00Z",
        }))
        .unwrap()
    }

    /// A peers handle with one host already connected as `peer-device`, and no
    /// tunnel tasks (tests never spawn ssh).
    fn peers_with(view: PeerView) -> SshPeers {
        let (state, _) = watch::channel(PeersState {
            peers: BTreeMap::from([("devbox".to_string(), view)]),
        });
        SshPeers {
            inner: Arc::new(Inner {
                known: Mutex::new(HashMap::new()),
                data_dir: PathBuf::from("/nonexistent"),
                local_device_id: "local".into(),
                state,
                clients: Mutex::new(HashMap::new()),
                tasks: Mutex::new(HashMap::new()),
                routes: Mutex::new(HashMap::new()),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    fn connected_peer() -> SshPeers {
        peers_with(PeerView {
            device_id: Some("peer".into()),
            connected: true,
            error: None,
            devices: vec![device("peer")],
            spaces: vec![space("space-peer", "peer")],
            chats: vec![chat("chat-peer", "peer")],
            sessions: Vec::new(),
        })
    }

    #[test]
    fn hosts_come_from_the_settings_file_deduped_and_validated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"[{"host":"devbox","addedAt":"2026-09-26T17:54:26Z"},
                {"host":"-oProxyCommand=evil"},{"host":"devbox"},{"host":"box"}]"#,
        )
        .unwrap();
        assert_eq!(configured_hosts(dir.path()), ["devbox", "box"]);
        assert!(configured_hosts(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn calls_route_to_the_peer_that_owns_their_rows() {
        let peers = connected_peer();
        let route = |method: &str, params: Value, forwardable: bool| {
            peers.route(method, &params, forwardable)
        };
        // Explicit targets: only forwardable methods, only peers.
        assert_eq!(
            route(
                methods::LIST_FOLDERS,
                serde_json::json!({"targetDeviceId": "peer"}),
                true
            )
            .as_deref(),
            Some("peer")
        );
        assert_eq!(
            route("X", serde_json::json!({"targetDeviceId": "peer"}), false),
            None
        );
        assert_eq!(
            route(
                methods::LIST_FOLDERS,
                serde_json::json!({"targetDeviceId": "local"}),
                true
            ),
            None
        );
        // Chat-scoped calls follow the chat's host, subagent docs included.
        for chat_id in ["chat-peer", "chat-peer--sub--toolu_1"] {
            assert_eq!(
                route(
                    methods::WATCH_DOC_MESSAGES,
                    serde_json::json!({"chatId": chat_id}),
                    true
                )
                .as_deref(),
                Some("peer")
            );
        }
        assert_eq!(
            route(
                methods::QUEUE_COMMAND,
                serde_json::json!({"chatId": "chat-local"}),
                true
            ),
            None
        );
        assert_eq!(
            route(
                "ListProjectActions",
                serde_json::json!({"spaceId": "space-peer"}),
                true
            )
            .as_deref(),
            Some("peer")
        );
        // Mutates: by the row they touch; sidebar pins stay local.
        let mutate = |params: Value| route(methods::MUTATE, params, false);
        assert_eq!(
            mutate(serde_json::json!({"op": "renameChat", "chatId": "chat-peer", "title": "t"}))
                .as_deref(),
            Some("peer")
        );
        assert_eq!(
            mutate(serde_json::json!({"op": "deleteSpace", "spaceId": "space-peer"})).as_deref(),
            Some("peer")
        );
        assert_eq!(
            mutate(serde_json::json!({"op": "createSpace", "spaceId": "s2", "deviceId": "peer", "path": "/x"}))
                .as_deref(),
            Some("peer")
        );
        assert_eq!(
            mutate(serde_json::json!({"op": "renameChat", "chatId": "chat-local", "title": "t"})),
            None
        );
        assert_eq!(
            mutate(serde_json::json!({"op": "changeSidebarPin", "chatId": "chat-peer"})),
            None
        );
    }

    #[test]
    fn a_chat_minted_on_a_peer_routes_there_before_its_row_arrives() {
        let peers = connected_peer();
        let created = peers.route(
            methods::MUTATE,
            &serde_json::json!({"op": "createChat", "chatId": "new-chat", "spaceId": "space-peer"}),
            false,
        );
        assert_eq!(created.as_deref(), Some("peer"));
        assert_eq!(
            peers
                .route(
                    methods::QUEUE_COMMAND,
                    &serde_json::json!({"chatId": "new-chat"}),
                    true
                )
                .as_deref(),
            Some("peer")
        );
        // A project-less chat names the device outright.
        assert_eq!(
            peers
                .route(
                    methods::MUTATE,
                    &serde_json::json!({"op": "createChat", "chatId": "c3", "deviceId": "peer"}),
                    false
                )
                .as_deref(),
            Some("peer")
        );
        assert_eq!(
            peers.route(
                methods::MUTATE,
                &serde_json::json!({"op": "createChat", "chatId": "c4", "deviceId": "local"}),
                false
            ),
            None
        );
    }

    #[tokio::test]
    async fn a_known_host_routes_before_it_reconnects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), r#"[{"host":"devbox"}]"#).unwrap();
        std::fs::write(dir.path().join(KNOWN_FILE), r#"{"devbox":"peer"}"#).unwrap();
        let peers = SshPeers::start(dir.path().to_path_buf(), "local".into());
        let route = peers.route(
            methods::MUTATE,
            &serde_json::json!({"op": "createChat", "chatId": "c", "deviceId": "peer"}),
            false,
        );
        assert_eq!(route.as_deref(), Some("peer"));
        assert!(
            peers.client("peer").is_err(),
            "offline until the tunnel is up"
        );
        peers.shutdown();
    }

    #[tokio::test]
    async fn a_new_chats_watch_waits_for_the_createchat_that_names_its_host() {
        let peers = connected_peer();
        let waiting = {
            let peers = peers.clone();
            tokio::spawn(async move {
                peers
                    .await_chat_owner("fresh", |_| false, Duration::from_secs(5))
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        peers.route(
            methods::MUTATE,
            &serde_json::json!({"op": "createChat", "chatId": "fresh", "spaceId": "space-peer"}),
            false,
        );
        assert_eq!(waiting.await.unwrap().as_deref(), Some("peer"));
        // A chat this engine knows resolves at once; an unclaimed one gives up.
        assert_eq!(
            peers
                .await_chat_owner("mine", |id| id == "mine", Duration::from_secs(5))
                .await,
            None
        );
        let started = std::time::Instant::now();
        assert_eq!(
            peers
                .await_chat_owner("nobody", |_| false, Duration::from_millis(200))
                .await,
            None
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_disconnected_peer_has_no_client_and_says_why() {
        let peers = connected_peer();
        let Err(err) = peers.client("peer") else {
            panic!("a peer without a connection has no client");
        };
        let err = err.to_string();
        assert!(err.contains("devbox"), "{err}");
    }

    #[tokio::test]
    async fn merged_watches_add_peer_rows_and_keep_connected_peers_present() {
        let peers = connected_peer();
        let (local_tx, local_rx) = watch::channel(vec![device("local")]);
        let merged = peers.merged_devices(local_rx);
        let rows = merged.borrow().clone();
        let ids: Vec<&str> = rows.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(ids, ["local", "peer"]);
        assert!(
            rows[1].last_seen_at.is_some(),
            "a connected peer reads as present"
        );

        let (_chats_tx, chats_rx) = watch::channel(vec![chat("chat-local", "local")]);
        let mut chats = peers.merged_chats(chats_rx);
        assert_eq!(chats.borrow().len(), 2);

        // A dropped tunnel keeps the rows but lets presence lapse.
        peers.inner.state.send_modify(|state| {
            state.peers.get_mut("devbox").unwrap().connected = false;
            state.peers.get_mut("devbox").unwrap().chats.clear();
        });
        tokio::time::timeout(Duration::from_secs(2), chats.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(chats.borrow().len(), 1);
        drop(local_tx);
    }

    #[test]
    fn a_peer_row_replaces_a_stale_local_copy() {
        let merged = merge_rows(
            &[chat("shared", "local"), chat("mine", "local")],
            vec![chat("shared", "peer")],
            |c| &c.id,
        );
        let owners: Vec<(&str, &str)> = merged
            .iter()
            .map(|c| (c.id.as_str(), c.device_id.as_str()))
            .collect();
        assert_eq!(owners, [("mine", "local"), ("shared", "peer")]);
    }
}
