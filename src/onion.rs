//! Arti onion service + outbound Tor client on a dedicated OS thread.
//!
//! Auto-retries forever on failure. Handles rend requests concurrently with
//! status (never blocks accept behind fully_reachable).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures::StreamExt;
use safelog::DisplayRedacted;
use tor_cell::relaycell::msg::Connected;
use tor_hsservice::config::OnionServiceConfigBuilder;
use tor_proto::stream::IncomingStreamRequest;
use tracing::{error, info, warn};

use arti_client::config::TorClientConfigBuilder;
use arti_client::TorClient;
use tor_rtcompat::PreferredRuntime;

use crate::crypto::now_unix_ms;
use crate::db::{Contact, ContactStatus, Message, OutboxItem};
use crate::http::{
    build_http_post, handle_api, read_http_request, read_http_status, write_error, AppShared,
};
use crate::identity::{ensure_private_dir, onion_address_from_id, onion_id_from_address};
use crate::protocol::{IntroAckFrame, IntroFrame, Invite, MsgFrame};

/// Events pushed to the UI / CLI.
#[derive(Debug, Clone)]
pub enum OnionEvent {
    Status(String),
    /// Full `*.onion` hostname (UI strips to 56-char for display).
    OnionAddress(String),
    /// 56-char transport id (without .onion).
    OnionId(String),
    Ready,
    /// Tor/onion is down (will auto-retry).
    Down(String),
    Error(String),
    /// Hint for UI to reload DB-backed views.
    DbChanged,
}

/// Commands from UI → onion thread.
#[derive(Debug, Clone)]
pub enum OnionCommand {
    /// Send INTRO to peer (from scanned/pasted invite).
    SendIntro { invite: Invite },
    /// Accept or reject a contact request (sends ACK + updates DB).
    RespondRequest { rowid: i64, accept: bool },
    /// Queue / send a sealed text message.
    SendMessage { peer_id: String, text: String },
    /// Update local nick (persisted).
    SetNick { nick: String },
    /// Set custom nick for a contact.
    SetCustomNick {
        peer_id: String,
        nick: Option<String>,
    },
    /// Force outbox drain attempt.
    FlushOutbox,
}

fn send(tx: &Sender<OnionEvent>, ev: OnionEvent) {
    let _ = tx.send(ev);
}

/// Spawn Arti on a dedicated thread with its own multi-thread Tokio runtime.
/// Retries forever on failure. Returns the command sender for the UI.
pub fn spawn_onion_thread(
    event_tx: Sender<OnionEvent>,
    shared: Arc<AppShared>,
    data_root: impl AsRef<Path>,
) -> Sender<OnionCommand> {
    let data_root = data_root.as_ref().to_path_buf();
    let (cmd_tx, cmd_rx) = mpsc::channel::<OnionCommand>();

    std::thread::Builder::new()
        .name("arti-onion".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("arti-worker")
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    send(
                        &event_tx,
                        OnionEvent::Error(format!("failed to create Tokio runtime: {err}")),
                    );
                    return;
                }
            };

            rt.block_on(run_forever(event_tx, cmd_rx, shared, data_root));
        })
        .expect("spawn arti-onion thread");

    cmd_tx
}

async fn run_forever(
    event_tx: Sender<OnionEvent>,
    cmd_rx: Receiver<OnionCommand>,
    shared: Arc<AppShared>,
    data_root: PathBuf,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cmd_rx = Arc::new(tokio::sync::Mutex::new(cmd_rx));
    let mut backoff = Duration::from_secs(2);

    loop {
        send(
            &event_tx,
            OnionEvent::Status("Connecting to network…".into()),
        );

        match run_session(
            event_tx.clone(),
            cmd_rx.clone(),
            shared.clone(),
            data_root.clone(),
        )
        .await
        {
            Ok(()) => {
                send(
                    &event_tx,
                    OnionEvent::Down("Network unavailable; retrying…".into()),
                );
                backoff = Duration::from_secs(2);
            }
            Err(err) => {
                let msg = format!("{err:#}");
                error!(%msg, "onion session failed");
                send(&event_tx, OnionEvent::Down(msg.clone()));
                send(&event_tx, OnionEvent::Error(msg));
            }
        }

        send(
            &event_tx,
            OnionEvent::Status(format!(
                "Auto-retry in {}s…",
                backoff.as_secs()
            )),
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn run_session(
    event_tx: Sender<OnionEvent>,
    cmd_rx: Arc<tokio::sync::Mutex<Receiver<OnionCommand>>>,
    shared: Arc<AppShared>,
    data_root: PathBuf,
) -> Result<()> {
    let state_dir = data_root.join("arti-state");
    let cache_dir = data_root.join("arti-cache");
    ensure_private_dir(&state_dir)?;
    ensure_private_dir(&cache_dir)?;

    send(
        &event_tx,
        OnionEvent::Status("Connecting to network…".into()),
    );

    let config = TorClientConfigBuilder::from_directories(&state_dir, &cache_dir)
        .build()
        .context("build TorClientConfig")?;

    let client = TorClient::create_bootstrapped(config)
        .await
        .context("bootstrap TorClient")?;

    send(
        &event_tx,
        OnionEvent::Status("Network ready. Starting service…".into()),
    );

    let svc_cfg = OnionServiceConfigBuilder::default()
        .nickname(
            "onion-chat"
                .parse()
                .context("invalid HS nickname")?,
        )
        .build()
        .context("build OnionServiceConfig")?;

    let (service, request_stream) = client
        .launch_onion_service(svc_cfg)
        .context("launch_onion_service")?
        .ok_or_else(|| anyhow!("onion service disabled in Arti config"))?;

    if let Some(addr) = service.onion_address() {
        let onion = addr.display_unredacted().to_string();
        info!(%onion, "onion address");
        apply_onion_address(&event_tx, &shared, &onion)?;
    } else {
        warn!("onion address not yet available from keystore");
        send(
            &event_tx,
            OnionEvent::Status("Preparing identity…".into()),
        );
    }

    send(
        &event_tx,
        OnionEvent::Status("Publishing service…".into()),
    );

    // Status watcher — concurrent with rend handling.
    let tx_status = event_tx.clone();
    let service_for_status = service.clone();
    let status_task = tokio::spawn(async move {
        let mut statuses = service_for_status.status_events();
        let mut announced_ready = false;
        while let Some(status) = statuses.next().await {
            let state = status.state();
            send(
                &tx_status,
                OnionEvent::Status(format!("Service state: {state:?}")),
            );
            if !announced_ready && state.is_fully_reachable() {
                announced_ready = true;
                send(&tx_status, OnionEvent::Ready);
                send(
                    &tx_status,
                    OnionEvent::Status("Online — ready.".into()),
                );
            }
            if let Some(problem) = status.current_problem() {
                send(
                    &tx_status,
                    OnionEvent::Status(format!("Network issue (may be transient): {problem:?}")),
                );
            }
        }
    });

    // Soft ready after 30s so UI enables send even if fully_reachable is slow.
    let tx_soft = event_tx.clone();
    let soft_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        send(&tx_soft, OnionEvent::Ready);
        send(
            &tx_soft,
            OnionEvent::Status(
                "Soft-ready: try messaging.".into(),
            ),
        );
    });

    // Outbox + command processor.
    let client_for_cmd = client.clone();
    let shared_cmd = shared.clone();
    let tx_cmd = event_tx.clone();
    let cmd_rx2 = cmd_rx.clone();
    let cmd_task = tokio::spawn(async move {
        command_and_outbox_loop(client_for_cmd, shared_cmd, cmd_rx2, tx_cmd).await;
    });

    // Accept streams immediately (do NOT wait for fully_reachable).
    let stream_requests = tor_hsservice::handle_rend_requests(request_stream);
    futures::pin_mut!(stream_requests);

    while let Some(stream_request) = stream_requests.next().await {
        let shared = shared.clone();
        let tx = event_tx.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_stream_request(stream_request, shared, tx).await {
                error!(?err, "error serving onion stream");
            }
        });
    }

    // Session over: stop per-session tasks so the next session does not run
    // a second command/outbox loop against a dead TorClient.
    cmd_task.abort();
    status_task.abort();
    soft_task.abort();
    drop(service);
    Ok(())
}

fn apply_onion_address(
    event_tx: &Sender<OnionEvent>,
    shared: &AppShared,
    onion: &str,
) -> Result<()> {
    send(event_tx, OnionEvent::OnionAddress(onion.to_string()));
    let id = onion_id_from_address(onion)?;
    send(event_tx, OnionEvent::OnionId(id.clone()));
    {
        let mut identity = shared
            .identity
            .lock()
            .map_err(|_| anyhow!("identity lock"))?;
        identity.onion_id = Some(id.clone());
    }
    {
        let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;
        db.set_onion_id(&id)?;
        let identity = shared
            .identity
            .lock()
            .map_err(|_| anyhow!("identity lock"))?;
        db.save_identity(&identity)?;
    }
    send(event_tx, OnionEvent::DbChanged);
    Ok(())
}

async fn handle_stream_request(
    stream_request: tor_hsservice::StreamRequest,
    shared: Arc<AppShared>,
    event_tx: Sender<OnionEvent>,
) -> Result<()> {
    match stream_request.request() {
        IncomingStreamRequest::Begin(begin) if begin.port() == 80 => {
            let mut stream = stream_request
                .accept(Connected::new_empty())
                .await
                .context("accept BEGIN")?;

            let req = match read_http_request(&mut stream).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(?e, "bad http request");
                    let _ = write_error(&mut stream, 400, "Bad Request", "bad request").await;
                    return Ok(());
                }
            };

            let before = now_unix_ms();
            if let Err(e) = handle_api(&mut stream, &req, &shared).await {
                error!(?e, "handler error");
            }
            // If handler mutated DB, nudge UI (cheap signal on any POST).
            if req.method == "POST" && now_unix_ms() >= before {
                send(&event_tx, OnionEvent::DbChanged);
            }
            Ok(())
        }
        _ => {
            stream_request
                .shutdown_circuit()
                .context("shutdown non-http stream")?;
            Ok(())
        }
    }
}

/// Outbox flush scheduler: at most one flush task at a time; kicks that arrive
/// while one runs are coalesced into exactly one follow-up run.
struct Flusher {
    running: AtomicBool,
    again: AtomicBool,
    force_again: AtomicBool,
}

impl Flusher {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(false),
            again: AtomicBool::new(false),
            force_again: AtomicBool::new(false),
        })
    }

    /// Start a flush in the background (never blocks the command loop).
    /// `force` ignores per-item retry backoff (new message / explicit flush).
    fn kick(
        self: &Arc<Self>,
        client: &Arc<TorClient<PreferredRuntime>>,
        shared: &Arc<AppShared>,
        event_tx: &Sender<OnionEvent>,
        force: bool,
    ) {
        if self.running.swap(true, Ordering::AcqRel) {
            if force {
                self.force_again.store(true, Ordering::Release);
            }
            self.again.store(true, Ordering::Release);
            return;
        }
        let me = self.clone();
        let client = client.clone();
        let shared = shared.clone();
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            let mut force = force;
            loop {
                if let Err(e) = flush_outbox(client.as_ref(), &shared, &event_tx, force).await {
                    warn!(?e, "outbox flush error");
                }
                if me.again.swap(false, Ordering::AcqRel) {
                    force = me.force_again.swap(false, Ordering::AcqRel);
                    continue;
                }
                me.running.store(false, Ordering::Release);
                // A kick may have landed between the check above and the store.
                if me.again.swap(false, Ordering::AcqRel)
                    && !me.running.swap(true, Ordering::AcqRel)
                {
                    force = me.force_again.swap(false, Ordering::AcqRel);
                    continue;
                }
                break;
            }
        });
    }
}

/// Command processor + periodic outbox retry.
///
/// Runs inside the Arti Tokio runtime, i.e. on the `arti-onion` OS thread that
/// lives as long as the process — on Android the foreground service
/// (`NotChatService`) keeps that process alive while the UI is backgrounded or
/// the screen is off, so this same loop is the background retry path.
async fn command_and_outbox_loop(
    client: Arc<TorClient<PreferredRuntime>>,
    shared: Arc<AppShared>,
    cmd_rx: Arc<tokio::sync::Mutex<Receiver<OnionCommand>>>,
    event_tx: Sender<OnionEvent>,
) {
    let flusher = Flusher::new();
    // Commands are polled often (UI responsiveness); the outbox is retried on
    // a slower timer, with per-item backoff inside `flush_outbox`.
    let mut cmd_poll = tokio::time::interval(Duration::from_millis(250));
    cmd_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_flush: Option<tokio::time::Instant> = None;

    loop {
        cmd_poll.tick().await;

        // Non-blocking drain of commands.
        let cmds: Vec<OnionCommand> = {
            let guard = cmd_rx.lock().await;
            let mut v = Vec::new();
            while let Ok(cmd) = guard.try_recv() {
                v.push(cmd);
            }
            v
        };
        for cmd in cmds {
            match handle_command(&shared, &event_tx, cmd) {
                Ok(true) => flusher.kick(&client, &shared, &event_tx, true),
                Ok(false) => {}
                Err(e) => {
                    warn!(?e, "command failed");
                    send(&event_tx, OnionEvent::Status(format!("Command error: {e:#}")));
                }
            }
        }

        // Periodic outbox retry.
        let due = last_flush.map_or(true, |t| t.elapsed() >= OUTBOX_TICK);
        if due {
            last_flush = Some(tokio::time::Instant::now());
            flusher.kick(&client, &shared, &event_tx, false);
        }
    }
}

/// How often the outbox is scanned for due retries.
const OUTBOX_TICK: Duration = Duration::from_secs(15);
/// Upper bound for one outbound HTTP-over-onion attempt.
const POST_TIMEOUT: Duration = Duration::from_secs(90);

/// Per-item retry backoff: 15s, 30s, 1m, 2m, 4m, then every 5m.
fn retry_backoff_ms(attempts: i64) -> i64 {
    if attempts <= 0 {
        return 0;
    }
    let exp = (attempts - 1).min(5) as u32;
    (15_000i64 * 2i64.pow(exp)).min(5 * 60_000)
}

/// Apply a UI command. Returns `true` when the outbox should be flushed now.
fn handle_command(
    shared: &AppShared,
    event_tx: &Sender<OnionEvent>,
    cmd: OnionCommand,
) -> Result<bool> {
    match cmd {
        OnionCommand::SetNick { nick } => {
            {
                let mut identity = shared.identity.lock().map_err(|_| anyhow!("id lock"))?;
                identity.nick = nick.clone();
                let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;
                db.set_nick(&nick)?;
                db.save_identity(&identity)?;
            }
            send(event_tx, OnionEvent::DbChanged);
            send(event_tx, OnionEvent::Status("Nick updated.".into()));
            Ok(false)
        }
        OnionCommand::SetCustomNick { peer_id, nick } => {
            let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;
            db.set_custom_nick(&peer_id, nick.as_deref())?;
            send(event_tx, OnionEvent::DbChanged);
            Ok(false)
        }
        OnionCommand::SendIntro { invite } => {
            queue_intro(shared, &invite)?;
            send(event_tx, OnionEvent::DbChanged);
            Ok(true)
        }
        OnionCommand::RespondRequest { rowid, accept } => {
            respond_request(shared, rowid, accept)?;
            send(event_tx, OnionEvent::DbChanged);
            Ok(true)
        }
        OnionCommand::SendMessage { peer_id, text } => {
            queue_message(shared, &peer_id, &text)?;
            send(event_tx, OnionEvent::DbChanged);
            Ok(true)
        }
        OnionCommand::FlushOutbox => Ok(true),
    }
}

fn queue_intro(shared: &AppShared, invite: &Invite) -> Result<()> {
    let identity = shared.identity.lock().map_err(|_| anyhow!("id lock"))?;
    let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;

    // Need peer nonce from QR to authenticate INTRO; TOFU-only id cannot INTRO yet.
    let nonce = invite
        .nonce
        .ok_or_else(|| anyhow!("paste full QR URI (needs nonce); bare id is TOFU-only until QR"))?;

    let now = now_unix_ms();
    let pk = if let Some(pk) = invite.pk {
        pk
    } else {
        return Err(anyhow!("invite missing pk — paste full onionchat:v1 URI"));
    };

    // Stage pending outbound contact.
    if db.get_contact(&invite.id)?.is_none() {
        db.upsert_contact(&Contact {
            id: invite.id.clone(),
            pk,
            self_nick: invite.nick.clone().unwrap_or_default(),
            custom_nick: None,
            status: ContactStatus::Pending,
            created_at: now,
            updated_at: now,
        })?;
    }

    let frame = IntroFrame::build(&identity, &nonce, now)?;
    let payload = serde_json::to_string(&frame)?;
    db.enqueue_outbox(&invite.id, "intro", &payload)?;
    Ok(())
}

fn respond_request(shared: &AppShared, rowid: i64, accept: bool) -> Result<()> {
    let identity = shared.identity.lock().map_err(|_| anyhow!("id lock"))?;
    let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;

    let req = db
        .get_request(rowid)?
        .ok_or_else(|| anyhow!("request not found"))?;
    if req.status != "pending" {
        return Err(anyhow!("request already handled"));
    }

    let decision = if accept { "accept" } else { "reject" };
    let now = now_unix_ms();
    let status = if accept {
        ContactStatus::Accepted
    } else {
        ContactStatus::Rejected
    };

    db.set_request_status(rowid, if accept { "accepted" } else { "rejected" })?;
    db.upsert_contact(&Contact {
        id: req.from_id.clone(),
        pk: req.from_pk,
        self_nick: req.from_nick.clone(),
        custom_nick: None,
        status,
        created_at: now,
        updated_at: now,
    })?;

    let frame = IntroAckFrame::build(&identity, decision, now)?;
    let payload = serde_json::to_string(&frame)?;
    db.enqueue_outbox(&req.from_id, "intro_ack", &payload)?;
    Ok(())
}

fn queue_message(shared: &AppShared, peer_id: &str, text: &str) -> Result<()> {
    let identity = shared.identity.lock().map_err(|_| anyhow!("id lock"))?;
    let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;

    let contact = db
        .get_contact(peer_id)?
        .ok_or_else(|| anyhow!("unknown contact"))?;
    if contact.status != ContactStatus::Accepted {
        return Err(anyhow!("contact not accepted yet"));
    }

    let pk = crate::crypto::verifying_key_from_bytes(&contact.pk)?;
    let msg_id = uuid::Uuid::new_v4().to_string();
    let ts = now_unix_ms();
    let frame = MsgFrame::build(&identity, &pk, &msg_id, text, ts)?;
    let payload = serde_json::to_string(&frame)?;

    db.insert_message(&Message {
        msg_id: msg_id.clone(),
        peer_id: peer_id.to_string(),
        direction: "out".into(),
        plaintext: text.to_string(),
        ts,
        status: "queued".into(),
    })?;
    db.enqueue_outbox(peer_id, "msg", &payload)?;
    Ok(())
}

/// Try to deliver queued outbound frames.
///
/// Items are grouped per peer: peers are tried **concurrently** (one
/// unreachable onion no longer delays delivery to the others), items for one
/// peer go in order and the peer is skipped for this round after the first
/// failure. Unless `force`, items still inside their retry backoff are skipped.
async fn flush_outbox(
    client: &TorClient<PreferredRuntime>,
    shared: &AppShared,
    event_tx: &Sender<OnionEvent>,
    force: bool,
) -> Result<()> {
    let items = {
        let db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;
        db.list_outbox(50)?
    };
    if items.is_empty() {
        return Ok(());
    }

    let now = now_unix_ms();
    let mut per_peer: Vec<(String, Vec<OutboxItem>)> = Vec::new();
    for item in items {
        if let Some((_, v)) = per_peer.iter_mut().find(|(p, _)| *p == item.peer_id) {
            v.push(item);
        } else {
            per_peer.push((item.peer_id.clone(), vec![item]));
        }
    }
    // A peer is due if its head item is due (keeps per-peer ordering).
    per_peer.retain(|(_, v)| {
        let head = &v[0];
        force
            || head
                .last_attempt
                .map_or(true, |t| now - t >= retry_backoff_ms(head.attempts))
    });
    if per_peer.is_empty() {
        return Ok(());
    }

    let jobs = per_peer
        .into_iter()
        .map(|(_, items)| flush_peer(client, shared, event_tx, items));
    for r in futures::future::join_all(jobs).await {
        if let Err(e) = r {
            warn!(?e, "outbox peer flush error");
        }
    }
    Ok(())
}

async fn flush_peer(
    client: &TorClient<PreferredRuntime>,
    shared: &AppShared,
    event_tx: &Sender<OnionEvent>,
    items: Vec<OutboxItem>,
) -> Result<()> {
    for item in items {
        {
            let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;
            db.mark_outbox_attempt(item.id)?;
        }

        let path = match item.kind.as_str() {
            "intro" => "/v1/intro",
            "intro_ack" => "/v1/intro/ack",
            "msg" => "/v1/msg",
            other => {
                warn!(kind = other, "unknown outbox kind — dropping");
                let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;
                db.remove_outbox(item.id)?;
                continue;
            }
        };

        let attempt = tokio::time::timeout(
            POST_TIMEOUT,
            post_to_peer(client, &item.peer_id, path, item.payload.as_bytes()),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow!("timed out after {}s", POST_TIMEOUT.as_secs())));

        match attempt {
            Ok(status) if (200..300).contains(&status) || status == 409 => {
                // 409 = peer already has this frame (replay guard) → delivered.
                info!(id = item.id, kind = %item.kind, peer = %item.peer_id, status, "outbox delivered");
                let mut db = shared.db.lock().map_err(|_| anyhow!("db lock"))?;
                db.remove_outbox(item.id)?;
                if item.kind == "msg" {
                    // Best-effort: mark matching queued messages sent.
                    if let Ok(frame) = serde_json::from_str::<MsgFrame>(&item.payload) {
                        let _ = db.update_message_status(&frame.msg_id, "sent");
                    }
                }
                send(event_tx, OnionEvent::DbChanged);
            }
            Ok(status) => {
                warn!(status, peer = %item.peer_id, "peer returned non-2xx — will retry");
                send(
                    event_tx,
                    OnionEvent::Status(format!(
                        "Peer {} returned HTTP {status}; will retry.",
                        &item.peer_id[..8.min(item.peer_id.len())]
                    )),
                );
                break;
            }
            Err(e) => {
                warn!(?e, peer = %item.peer_id, "outbound failed — will retry");
                send(
                    event_tx,
                    OnionEvent::Status(format!(
                        "Peer unreachable ({e:#}); will retry."
                    )),
                );
                break;
            }
        }
    }
    Ok(())
}

async fn post_to_peer(
    client: &TorClient<PreferredRuntime>,
    peer_id: &str,
    path: &str,
    json_body: &[u8],
) -> Result<u16> {
    let host = onion_address_from_id(peer_id)?;
    let mut stream = client
        .connect((host.as_str(), 80))
        .await
        .with_context(|| format!("connect {host}"))?;

    let req = build_http_post(path, &host, json_body);
    use tokio::io::AsyncWriteExt;
    stream.write_all(&req).await.context("write request")?;
    stream.flush().await.ok();

    let (status, _) = read_http_status(&mut stream).await?;
    Ok(status)
}

/// Convenience: open DB + identity for headless / UI bootstrap.
pub fn open_app_state(data_root: &Path) -> Result<Arc<AppShared>> {
    ensure_private_dir(data_root)?;
    let mut db = crate::db::Db::open(data_root)?;
    let identity = db.load_or_create_identity()?;
    Ok(AppShared::new(db, identity))
}
