//! SSH peers, in process: engine `local` drives engine `peer` the way it does a
//! machine reached over an ssh tunnel — the peer's rows join `local`'s watches,
//! and chat work addressed to it runs there — with an in-memory client
//! standing in for the tunnel.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use zeron_engine::ssh_peers::SshPeers;
use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_harness::{Harness, HarnessError, RunControls};
use zeron_proto::{
    AgentEvent, DoneStatus, HarnessId, Model, ReasoningLevel, RunRequest, SteeringMode,
};
use zeron_rpc::{RpcClient, methods};

struct InstantHarness;

#[async_trait::async_trait]
impl Harness for InstantHarness {
    fn id(&self) -> HarnessId {
        HarnessId::Mock
    }
    fn display_name(&self) -> &str {
        "Instant"
    }
    fn supports_steering(&self) -> bool {
        false
    }
    fn steering_mode(&self) -> SteeringMode {
        SteeringMode::TurnBoundary
    }
    fn reasoning_levels(&self) -> &[ReasoningLevel] {
        &[]
    }
    async fn models(&self) -> Result<Vec<Model>, HarnessError> {
        Ok(vec![])
    }
    async fn run(
        &self,
        request: RunRequest,
        _controls: RunControls,
    ) -> Result<BoxStream<'static, Result<AgentEvent, HarnessError>>, HarnessError> {
        if request.prompt.contains("fail") {
            return Ok(futures::stream::iter([Ok(AgentEvent::Done {
                status: DoneStatus::Errored,
                result: None,
                error: Some("the model sign-in expired".into()),
                session_id: None,
            })])
            .boxed());
        }
        Ok(futures::stream::iter([
            Ok(AgentEvent::SessionStarted {
                harness: HarnessId::Mock,
                model: "instant-1".into(),
                tools: vec![],
                cwd: "/tmp".into(),
                session_id: "hs-1".into(),
                assistant_message_id: "a-1".into(),
            }),
            Ok(AgentEvent::TextDelta {
                text: "reply from the peer".into(),
            }),
            Ok(AgentEvent::Done {
                status: DoneStatus::Completed,
                result: None,
                error: None,
                session_id: Some("hs-1".into()),
            }),
        ])
        .boxed())
    }
}

fn assemble(dir: &std::path::Path, device_id: &str) -> EngineCore {
    std::fs::create_dir_all(dir).expect("create data dir");
    std::fs::write(dir.join("device-id"), device_id).expect("write device id");
    let registry = HarnessRegistry::new();
    registry.register(Arc::new(InstantHarness));
    EngineCore::assemble(dir, Arc::new(registry), HarnessId::Mock, None).expect("engine assembles")
}

/// Poll a watch stream until `found` matches one of its items.
async fn wait_for<T>(
    client: &RpcClient,
    method: &str,
    params: serde_json::Value,
    mut found: impl FnMut(&serde_json::Value) -> Option<T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut rx = client
                .subscribe(method, params.clone())
                .await
                .expect(method);
            while let Some(item) = rx.recv().await {
                if let Some(value) = found(&item) {
                    return value;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{method}: timed out"))
}

fn ids_on(item: &serde_json::Value, device: &str) -> Vec<String> {
    item.as_array()
        .into_iter()
        .flatten()
        .filter(|row| row["deviceId"] == device || row["id"] == device)
        .filter_map(|row| row["id"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_peer_is_listed_and_driven_through_the_local_engine() {
    let dirs = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let peer = assemble(dirs.0.path(), "peer");
    let local = assemble(dirs.1.path(), "local");
    let peers = SshPeers::start(dirs.1.path().join("no-ssh-devices"), "local".into());
    local.set_ssh_peers(peers.clone());
    let _stop = peers.attach(
        "devbox",
        Arc::new(zeron_rpc::memory_client(peer.rpc_service())),
    );
    let client = zeron_rpc::memory_client(local.rpc_service());
    let peer_client = zeron_rpc::memory_client(peer.rpc_service());

    // The peer's device joins the local list, present while connected.
    let seen = wait_for(
        &client,
        methods::WATCH_DEVICES,
        serde_json::json!({}),
        |item| {
            let rows = item.as_array()?;
            let peer = rows.iter().find(|row| row["id"] == "peer")?;
            rows.iter()
                .any(|row| row["id"] == "local")
                .then(|| peer["lastSeenAt"].clone())
        },
    )
    .await;
    assert!(
        seen.is_string(),
        "a connected peer reads as present: {seen}"
    );

    // A folder on the peer becomes a space there (the Mutate is routed by
    // its deviceId) and shows up through the local watch.
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace", "spaceId": "space-peer", "deviceId": "peer",
                "path": dirs.0.path().to_string_lossy(),
            }),
        )
        .await
        .expect("createSpace routes to the peer");
    wait_for(
        &client,
        methods::WATCH_SPACES,
        serde_json::json!({}),
        |item| {
            ids_on(item, "peer")
                .contains(&"space-peer".to_string())
                .then_some(())
        },
    )
    .await;
    let peer_spaces = wait_for(
        &peer_client,
        methods::WATCH_SPACES,
        serde_json::json!({}),
        |item| Some(ids_on(item, "peer")),
    )
    .await;
    assert_eq!(
        peer_spaces,
        ["space-peer"],
        "the space lives in the peer's registry"
    );

    // New chat on that space, then a run — both reach the peer, whose
    // harness answers; the transcript streams back through the local engine.
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat", "chatId": "chat-1", "spaceId": "space-peer",
                "config": {"harness": "mock", "model": null, "reasoning": null,
                           "sandbox": "workspace-write"},
            }),
        )
        .await
        .expect("createChat routes to the peer");
    client
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": "chat-1",
                "command": {"kind": "run", "messageId": "m-1", "request": {
                    "prompt": "hello", "harness": "mock", "model": null, "reasoning": null,
                    "cwd": dirs.0.path().to_string_lossy(), "sandbox": "workspace-write",
                    "autoApprove": true, "resume": null}},
            }),
        )
        .await
        .expect("QueueCommand routes to the peer");
    wait_for(
        &client,
        methods::WATCH_DOC_MESSAGES,
        serde_json::json!({"chatId": "chat-1", "openingTail": true}),
        |item| {
            let text = item.to_string();
            (text.contains("reply from the peer") && text.contains("\"peer\"")).then_some(())
        },
    )
    .await;

    // The chat row and its session are the peer's, seen locally.
    wait_for(
        &client,
        methods::WATCH_CHATS,
        serde_json::json!({}),
        |item| {
            ids_on(item, "peer")
                .contains(&"chat-1".to_string())
                .then_some(())
        },
    )
    .await;
    wait_for(
        &client,
        methods::WATCH_SESSIONS,
        serde_json::json!({}),
        |item| {
            item.as_array()?
                .iter()
                .any(|row| row["chatId"] == "chat-1" && row["deviceId"] == "peer")
                .then_some(())
        },
    )
    .await;

    // Renames and deletes land in the peer's registry, not the local one.
    client
        .call(
            methods::MUTATE,
            serde_json::json!({"op": "renameChat", "chatId": "chat-1", "title": "Renamed"}),
        )
        .await
        .expect("renameChat routes to the peer");
    wait_for(
        &peer_client,
        methods::WATCH_CHATS,
        serde_json::json!({}),
        |item| {
            item.as_array()?
                .iter()
                .any(|row| row["id"] == "chat-1" && row["title"] == "Renamed")
                .then_some(())
        },
    )
    .await;
    assert!(
        local
            .workspace
            .read_chats()
            .unwrap_or_default()
            .iter()
            .all(|chat| chat.id != "chat-1"),
        "the peer's chat is never written into the local registry"
    );
    client
        .call(
            methods::MUTATE,
            serde_json::json!({"op": "deleteChat", "chatId": "chat-1"}),
        )
        .await
        .expect("deleteChat routes to the peer");
    wait_for(
        &client,
        methods::WATCH_CHATS,
        serde_json::json!({}),
        |item| (!ids_on(item, "peer").contains(&"chat-1".to_string())).then_some(()),
    )
    .await;

    peers.shutdown();
    peer.shutdown().await;
    local.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disconnected_peers_chats_fail_clearly_instead_of_running_locally() {
    let dirs = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let peer = assemble(dirs.0.path(), "peer");
    let local = assemble(dirs.1.path(), "local");
    let peers = SshPeers::start(dirs.1.path().join("no-ssh-devices"), "local".into());
    local.set_ssh_peers(peers.clone());
    let stop = peers.attach(
        "devbox",
        Arc::new(zeron_rpc::memory_client(peer.rpc_service())),
    );
    let client = zeron_rpc::memory_client(local.rpc_service());
    wait_for(
        &client,
        methods::WATCH_DEVICES,
        serde_json::json!({}),
        |item| {
            item.as_array()?
                .iter()
                .any(|row| row["id"] == "peer")
                .then_some(())
        },
    )
    .await;
    client
        .call(
            methods::MUTATE,
            serde_json::json!({"op": "createChat", "chatId": "chat-2", "deviceId": "peer"}),
        )
        .await
        .expect("createChat routes to the peer");
    wait_for(
        &client,
        methods::WATCH_CHATS,
        serde_json::json!({}),
        |item| {
            ids_on(item, "peer")
                .contains(&"chat-2".to_string())
                .then_some(())
        },
    )
    .await;

    // The tunnel drops: the row stays, calls for it fail with the host's name.
    stop.cancel();
    let mut state = peers.watch();
    tokio::time::timeout(Duration::from_secs(10), async {
        while state.borrow_and_update().peers["devbox"].connected {
            state.changed().await.expect("peers state");
        }
    })
    .await
    .expect("the peer reads as disconnected");
    let rows = wait_for(
        &client,
        methods::WATCH_CHATS,
        serde_json::json!({}),
        |item| Some(ids_on(item, "peer")),
    )
    .await;
    assert!(
        rows.contains(&"chat-2".to_string()),
        "rows outlive the tunnel"
    );

    let err = tokio::time::timeout(
        Duration::from_secs(10),
        client.call(
            methods::QUEUE_MESSAGE,
            serde_json::json!({"chatId": "chat-2", "text": "hi"}),
        ),
    )
    .await
    .expect("fails fast")
    .expect_err("a disconnected peer's chat never runs locally")
    .to_string();
    assert!(err.contains("devbox"), "{err}");
    assert!(
        local
            .workspace
            .read_chats()
            .unwrap_or_default()
            .iter()
            .all(|chat| chat.id != "chat-2"),
        "nothing about the peer's chat was created locally"
    );

    peers.shutdown();
    peer.shutdown().await;
    local.shutdown().await;
}

/// The composer's real order for a new chat on another machine: it selects
/// the chat — opening its transcript and queue watches — before the
/// createChat that says where it lives, then queues the run. "Sending…"
/// clears when the sent message id shows up in that transcript watch.
async fn send_like_the_composer(
    client: &RpcClient,
    chat_id: &str,
    space_id: &str,
    message_id: &str,
    prompt: &str,
    cwd: &str,
) -> tokio::sync::mpsc::Receiver<serde_json::Value> {
    let transcript = client
        .subscribe(
            methods::WATCH_DOC_MESSAGES,
            serde_json::json!({"chatId": chat_id, "openingTail": true}),
        )
        .await
        .expect("transcript watch");
    let _queue = client
        .subscribe(methods::WATCH_QUEUE, serde_json::json!({"chatId": chat_id}))
        .await
        .expect("queue watch");
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat", "chatId": chat_id, "spaceId": space_id,
                "config": {"harness": "mock", "model": null, "reasoning": null,
                           "sandbox": "workspace-write"},
            }),
        )
        .await
        .expect("createChat");
    client
        .call(
            methods::QUEUE_COMMAND,
            serde_json::json!({
                "chatId": chat_id,
                "command": {"kind": "run", "messageId": message_id, "request": {
                    "prompt": prompt, "harness": "mock", "model": null, "reasoning": null,
                    "cwd": cwd, "sandbox": "workspace-write", "autoApprove": false,
                    "resume": null, "attachments": []}},
            }),
        )
        .await
        .expect("QueueCommand");
    transcript
}

/// Materialize transcript frames until `done` holds for the entries.
async fn follow_transcript(
    rx: &mut tokio::sync::mpsc::Receiver<serde_json::Value>,
    done: impl Fn(&[zeron_doc::SessionMessageEntry]) -> bool,
) -> Vec<zeron_doc::SessionMessageEntry> {
    let mut entries = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(item) = rx.recv().await {
            let frame: zeron_doc::TranscriptFrame =
                serde_json::from_value(item).expect("transcript frame");
            zeron_doc::apply_transcript_frame(&mut entries, frame).expect("frame applies");
            if done(&entries) {
                return;
            }
        }
        panic!("the transcript watch ended before the send was acknowledged");
    })
    .await
    .expect("the sent message reaches the transcript watch opened before createChat");
    entries
}

#[tokio::test(flavor = "multi_thread")]
async fn a_composer_send_to_a_peer_is_acknowledged_even_when_its_turn_fails() {
    let dirs = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let peer = assemble(dirs.0.path(), "peer");
    let local = assemble(dirs.1.path(), "local");
    let peers = SshPeers::start(dirs.1.path().join("no-ssh-devices"), "local".into());
    local.set_ssh_peers(peers.clone());
    let _stop = peers.attach(
        "devbox",
        Arc::new(zeron_rpc::memory_client(peer.rpc_service())),
    );
    let client = zeron_rpc::memory_client(local.rpc_service());
    let cwd = dirs.0.path().to_string_lossy().to_string();
    // The device menu lists the peer only once it is connected.
    wait_for(
        &client,
        methods::WATCH_DEVICES,
        serde_json::json!({}),
        |item| {
            item.as_array()?
                .iter()
                .any(|row| row["id"] == "peer")
                .then_some(())
        },
    )
    .await;
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace", "spaceId": "space-peer", "deviceId": "peer", "path": cwd,
            }),
        )
        .await
        .expect("createSpace");
    wait_for(
        &client,
        methods::WATCH_SPACES,
        serde_json::json!({}),
        |item| {
            ids_on(item, "peer")
                .contains(&"space-peer".to_string())
                .then_some(())
        },
    )
    .await;

    // A turn that succeeds: the ack (the message id) and the reply arrive.
    let mut rx =
        send_like_the_composer(&client, "chat-ok", "space-peer", "msg-ok", "hello", &cwd).await;

    let entries = follow_transcript(&mut rx, |entries| {
        entries.iter().any(|e| e.id == "msg-ok")
            && entries.iter().any(|e| {
                serde_json::to_string(e)
                    .unwrap()
                    .contains("reply from the peer")
            })
    })
    .await;
    assert!(entries.iter().all(|e| e.device_id == "peer"), "{entries:?}");

    // A turn that fails on the peer still acknowledges the send, and the
    // failure reaches the transcript.
    let mut rx =
        send_like_the_composer(&client, "chat-err", "space-peer", "msg-err", "fail", &cwd).await;
    follow_transcript(&mut rx, |entries| {
        entries.iter().any(|e| e.id == "msg-err")
            && entries.iter().any(|e| {
                serde_json::to_string(e)
                    .unwrap()
                    .contains("the model sign-in expired")
            })
    })
    .await;

    // Nothing about these chats was opened or written on this machine.
    for chat in ["chat-ok", "chat-err"] {
        assert!(
            local.workspace.chat(chat).unwrap().is_none(),
            "{chat} has no local row"
        );
    }

    peers.shutdown();
    peer.shutdown().await;
    local.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_local_chat_still_opens_locally_with_peers_configured() {
    let dirs = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let peer = assemble(dirs.0.path(), "peer");
    let local = assemble(dirs.1.path(), "local");
    let peers = SshPeers::start(dirs.1.path().join("no-ssh-devices"), "local".into());
    local.set_ssh_peers(peers.clone());
    let _stop = peers.attach(
        "devbox",
        Arc::new(zeron_rpc::memory_client(peer.rpc_service())),
    );
    let client = zeron_rpc::memory_client(local.rpc_service());
    let cwd = dirs.1.path().to_string_lossy().to_string();
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createSpace", "spaceId": "space-local", "deviceId": "local", "path": cwd,
            }),
        )
        .await
        .expect("createSpace");
    let started = std::time::Instant::now();
    let mut rx = send_like_the_composer(
        &client,
        "chat-here",
        "space-local",
        "msg-here",
        "hello",
        &cwd,
    )
    .await;
    let entries = follow_transcript(&mut rx, |entries| {
        entries.iter().any(|e| e.id == "msg-here")
            && entries.iter().any(|e| {
                serde_json::to_string(e)
                    .unwrap()
                    .contains("reply from the peer")
            })
    })
    .await;
    assert!(
        entries.iter().all(|e| e.device_id == "local"),
        "{entries:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "a local chat must not wait out the peer-owner deadline"
    );

    peers.shutdown();
    peer.shutdown().await;
    local.shutdown().await;
}
