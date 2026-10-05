//! Process-wide singleton for the onion/HTTP/outbox runtime.
//!
//! On Android the process can outlive the Activity: the foreground service
//! (`NotChatService`) or the periodic backup job keeps it alive after the UI is
//! closed, and tao re-runs `main()` every time `MainActivity` is re-created.
//! Without a singleton every new Activity would open a second SQLite handle and
//! launch a second Arti instance on the same state dir (lock conflict, two
//! outbox loops). Instead:
//!
//! - `ensure_started` opens DB + identity and spawns the Arti thread **once**
//!   per process (called by the UI *and* by the Android service / job via JNI).
//! - Onion events go through a forwarder thread that keeps a small snapshot
//!   (ready / status / onion id) and forwards to the *current* UI subscriber.
//!   A freshly created UI calls `subscribe()` and immediately gets the snapshot
//!   replayed, so it does not sit on "Network unavailable" forever.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Result;
use tracing::info;

use crate::http::AppShared;
use crate::onion::{open_app_state, spawn_onion_thread, OnionCommand, OnionEvent};

#[derive(Default, Clone)]
struct NetSnapshot {
    ready: bool,
    status: Option<String>,
    onion_address: Option<String>,
    onion_id: Option<String>,
}

pub struct Runtime {
    pub shared: Arc<AppShared>,
    cmd_tx: Mutex<Sender<OnionCommand>>,
    ui_sink: Arc<Mutex<Option<Sender<OnionEvent>>>>,
    snapshot: Arc<Mutex<NetSnapshot>>,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static INIT_LOCK: Mutex<()> = Mutex::new(());

/// Start (once per process) and return the global runtime.
pub fn ensure_started(data_root: &Path) -> Result<&'static Runtime> {
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt);
    }
    let _guard = INIT_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(rt) = RUNTIME.get() {
        return Ok(rt);
    }

    let shared = open_app_state(data_root)?;
    let (ev_tx, ev_rx) = mpsc::channel::<OnionEvent>();
    let cmd_tx = spawn_onion_thread(ev_tx, shared.clone(), data_root);

    let ui_sink: Arc<Mutex<Option<Sender<OnionEvent>>>> = Arc::new(Mutex::new(None));
    let snapshot: Arc<Mutex<NetSnapshot>> = Arc::new(Mutex::new(NetSnapshot::default()));

    {
        let ui_sink = ui_sink.clone();
        let snapshot = snapshot.clone();
        std::thread::Builder::new()
            .name("onion-events".into())
            .spawn(move || forward_events(ev_rx, ui_sink, snapshot))
            .expect("spawn onion-events thread");
    }

    let _ = RUNTIME.set(Runtime {
        shared,
        cmd_tx: Mutex::new(cmd_tx),
        ui_sink,
        snapshot,
    });
    info!(root = %data_root.display(), "NotChat runtime started");
    Ok(RUNTIME.get().expect("runtime just set"))
}

/// Runtime if already started in this process.
pub fn get() -> Option<&'static Runtime> {
    RUNTIME.get()
}

impl Runtime {
    /// Clone of the command sender (UI keeps it in its own slot).
    pub fn cmd_sender(&self) -> Sender<OnionCommand> {
        self.cmd_tx
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub fn dispatch(&self, cmd: OnionCommand) {
        let _ = self.cmd_sender().send(cmd);
    }

    /// Become the (single) UI event subscriber. Replays the current network
    /// snapshot first so a re-created UI shows the right state immediately.
    pub fn subscribe(&self) -> Receiver<OnionEvent> {
        let (tx, rx) = mpsc::channel::<OnionEvent>();
        // Hold the sink lock while replaying so no live event slips in between.
        let mut sink = self.ui_sink.lock().unwrap_or_else(|p| p.into_inner());
        let snap = self
            .snapshot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(addr) = snap.onion_address {
            let _ = tx.send(OnionEvent::OnionAddress(addr));
        }
        if let Some(id) = snap.onion_id {
            let _ = tx.send(OnionEvent::OnionId(id));
        }
        if let Some(s) = snap.status {
            let _ = tx.send(OnionEvent::Status(s));
        }
        if snap.ready {
            let _ = tx.send(OnionEvent::Ready);
        }
        let _ = tx.send(OnionEvent::DbChanged);
        *sink = Some(tx);
        rx
    }
}

fn forward_events(
    rx: Receiver<OnionEvent>,
    ui_sink: Arc<Mutex<Option<Sender<OnionEvent>>>>,
    snapshot: Arc<Mutex<NetSnapshot>>,
) {
    let mut last_ready: Option<bool> = None;
    while let Ok(ev) = rx.recv() {
        let ready_now = {
            let mut snap = snapshot.lock().unwrap_or_else(|p| p.into_inner());
            match &ev {
                OnionEvent::Status(s) => snap.status = Some(s.clone()),
                OnionEvent::OnionAddress(a) => snap.onion_address = Some(a.clone()),
                OnionEvent::OnionId(id) => snap.onion_id = Some(id.clone()),
                OnionEvent::Ready => snap.ready = true,
                OnionEvent::Down(s) | OnionEvent::Error(s) => {
                    snap.ready = false;
                    snap.status = Some(s.clone());
                }
                OnionEvent::DbChanged => {}
            }
            snap.ready
        };
        if last_ready != Some(ready_now) {
            last_ready = Some(ready_now);
            #[cfg(target_os = "android")]
            crate::android::set_service_status(if ready_now {
                "Online — receiving messages"
            } else {
                "Connecting… (auto-retry)"
            });
        }
        let mut sink = ui_sink.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = sink.as_ref() {
            if tx.send(ev).is_err() {
                // UI went away (Activity destroyed); keep running headless.
                *sink = None;
            }
        }
    }
}
