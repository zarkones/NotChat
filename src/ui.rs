//! Dioxus UI screens for NotChat (onion-chat-v0) — Signal-like chats-first layout.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dioxus::document;
use dioxus::prelude::*;

use crate::crypto::now_unix_ms;
use crate::db::{Contact, ContactRequest, Message};
use crate::http::AppShared;
use crate::identity::default_data_root;
use crate::onion::{OnionCommand, OnionEvent};
use crate::protocol::Invite;
use crate::qr_display::invite_qr_data_uri;

#[derive(Clone, PartialEq)]
enum Screen {
    Chats,
    Chat { peer_id: String, title: String },
    ContactDetail { peer_id: String, title: String },
    Settings,
    AddContact,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NavAnim {
    Forward,
    Back,
    None,
}

type CmdTx = Arc<Mutex<Option<Sender<OnionCommand>>>>;

#[derive(Clone)]
struct CmdTxContext(CmdTx);

#[derive(Clone)]
struct SharedContext(Arc<AppShared>);

fn dispatch(cmd_slot: &CmdTx, cmd: OnionCommand) {
    if let Ok(guard) = cmd_slot.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(cmd);
        }
    }
}

fn navigate(mut screen: Signal<Screen>, mut nav_anim: Signal<NavAnim>, next: Screen, dir: NavAnim) {
    nav_anim.set(dir);
    screen.set(next);
}

/// Relative time for list rows: `22m`, `7h`, `2d`, …
fn relative_time(ts_ms: i64) -> String {
    let now = now_unix_ms();
    let secs = ((now - ts_ms).max(0)) / 1000;
    if secs < 60 {
        format!("{}s", secs.max(1))
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 86_400 * 7 {
        format!("{}d", secs / 86_400)
    } else {
        format!("{}w", secs / (86_400 * 7))
    }
}

fn preview_text(msg: &Message) -> String {
    let prefix = if msg.direction == "out" { "You: " } else { "" };
    let body = msg.plaintext.replace('\n', " ");
    let clipped = if body.chars().count() > 64 {
        let t: String = body.chars().take(61).collect();
        format!("{t}…")
    } else {
        body
    };
    format!("{prefix}{clipped}")
}

/// Delivery ticks for outbound messages only (no read receipts).
fn outbound_ticks(status: &str) -> &'static str {
    match status {
        "sent" | "delivered" => "✓✓",
        _ => "✓", // queued / sending / pending / unknown → single tick
    }
}

/// In-app brand mark (PNG data URI). Prefer assets/logo.png / logo-bar.png.
fn brand_logo_src() -> &'static str {
    use base64::Engine;
    use std::sync::OnceLock;
    static SRC: OnceLock<String> = OnceLock::new();
    SRC.get_or_init(|| {
        // Compact bar mark; falls back compile-time to full logo.png if bar missing.
        let bytes: &[u8] = include_bytes!("../assets/logo-bar.png");
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    })
    .as_str()
}

fn brand_logo_large_src() -> &'static str {
    use base64::Engine;
    use std::sync::OnceLock;
    static SRC: OnceLock<String> = OnceLock::new();
    SRC.get_or_init(|| {
        let bytes: &[u8] = include_bytes!("../assets/logo.png");
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    })
    .as_str()
}

fn persist_draft(shared: &crate::http::AppShared, peer_id: &str, text: &str) {
    if let Ok(mut db) = shared.db.lock() {
        let _ = db.set_draft(peer_id, text);
    }
}

fn load_draft(shared: &crate::http::AppShared, peer_id: &str) -> String {
    if let Ok(db) = shared.db.lock() {
        if let Ok(Some(t)) = db.get_draft(peer_id) {
            return t;
        }
    }
    String::new()
}

/// Strip Tor/onion/Arti jargon from strings that reach the UI status surface.
fn ui_net_status(s: &str) -> String {
    let lower = s.to_lowercase();
    if lower.contains("tor") || lower.contains("onion") || lower.contains("arti") {
        if lower.contains("retry") || lower.contains("down") || lower.contains("fail") {
            return "Network unavailable; retrying…".into();
        }
        return "Connecting to network…".into();
    }
    s.to_string()
}

fn screen_key(screen: &Screen) -> &'static str {
    match screen {
        Screen::Chats => "chats",
        Screen::Chat { .. } => "chat",
        Screen::ContactDetail { .. } => "contact",
        Screen::Settings => "settings",
        Screen::AddContact => "add",
    }
}

pub fn launch_app() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,onion_chat=info")
            }),
        )
        // try_init: on Android tao re-runs main() when the Activity is
        // re-created inside a process kept alive by the foreground service.
        .try_init()
        .ok();

    dioxus::launch(App);
}

#[component]
fn App() -> Element {
    let data = use_hook(default_data_root);
    // Process-wide runtime (DB + Arti + outbox). On Android it may already be
    // running from the foreground service / a previous Activity instance.
    let shared: Arc<AppShared> = use_hook(|| {
        crate::runtime::ensure_started(&data)
            .expect("open app state")
            .shared
            .clone()
    });

    let mut screen = use_signal(|| Screen::Chats);
    let mut nav_anim = use_signal(|| NavAnim::None);
    let mut net_ready = use_signal(|| false);
    let mut net_down = use_signal(|| true);
    let mut net_status = use_signal(|| "Connecting to network…".to_string());
    let mut dialog_dismissed = use_signal(|| false);
    let mut ok_banner_visible = use_signal(|| false);
    let mut ok_banner_gen = use_signal(|| 0u64);
    let mut onion_id = use_signal(String::new);
    let mut my_nick = use_signal(String::new);
    let mut qr_uri = use_signal(String::new);
    let mut toast = use_signal(String::new);

    let mut requests = use_signal(Vec::<ContactRequest>::new);
    let mut conversations = use_signal(Vec::<(Contact, Option<Message>)>::new);
    let mut messages = use_signal(Vec::<Message>::new);

    let mut draft = use_signal(String::new);
    let mut add_field = use_signal(String::new);
    let mut nick_edit = use_signal(String::new);
    let mut custom_nick_edit = use_signal(String::new);

    let mut scanning = use_signal(|| false);

    let rx_slot: Arc<Mutex<Option<Receiver<OnionEvent>>>> =
        use_hook(|| Arc::new(Mutex::new(None)));
    let cmd_slot: CmdTx = use_hook(|| Arc::new(Mutex::new(None)));

    let shared_boot = shared.clone();
    let rx_slot_boot = rx_slot.clone();
    let cmd_slot_boot = cmd_slot.clone();
    use_hook(move || {
        if let Ok(id) = shared_boot.identity.lock() {
            my_nick.set(id.nick.clone());
            nick_edit.set(id.nick.clone());
            if let Some(oid) = &id.onion_id {
                onion_id.set(oid.clone());
            }
            if let Ok(uri) = Invite::from_identity(&id) {
                qr_uri.set(uri);
            }
        }
        reload_lists(&shared_boot, &mut requests, &mut conversations);

        if let Some(rt) = crate::runtime::get() {
            // Subscribing replays the current network state (Ready / id).
            let rx: Receiver<OnionEvent> = rt.subscribe();
            if let Ok(mut slot) = rx_slot_boot.lock() {
                *slot = Some(rx);
            }
            if let Ok(mut slot) = cmd_slot_boot.lock() {
                *slot = Some(rt.cmd_sender());
            }
        }
    });

    // Mirror "which chat is on screen" for notification suppression.
    use_effect(move || {
        let active = match &*screen.read() {
            Screen::Chat { peer_id, .. } => Some(peer_id.clone()),
            _ => None,
        };
        crate::notify::set_active_chat(active.as_deref());
    });

    let rx_for_poll = rx_slot.clone();
    let shared_poll = shared.clone();
    use_future(move || {
        let rx_for_poll = rx_for_poll.clone();
        let shared_poll = shared_poll.clone();
        async move {
            let mut poll_n: u64 = 0;
            loop {
                poll_n = poll_n.wrapping_add(1);
                // Notification tap → open that chat (checked ~every 450ms).
                if poll_n % 3 == 1 {
                    if let Some(peer) = crate::notify::take_pending_open_peer() {
                        open_chat_from_notification(
                            &shared_poll,
                            &peer,
                            screen,
                            nav_anim,
                            messages,
                            draft,
                            custom_nick_edit,
                        );
                    }
                }
                let batch = {
                    let mut out = Vec::new();
                    if let Ok(guard) = rx_for_poll.lock() {
                        if let Some(rx) = guard.as_ref() {
                            while let Ok(ev) = rx.try_recv() {
                                out.push(ev);
                            }
                        }
                    }
                    out
                };
                for ev in batch {
                    match ev {
                        OnionEvent::Status(s) => net_status.set(ui_net_status(&s)),
                        OnionEvent::OnionAddress(_) => {}
                        OnionEvent::OnionId(id) => {
                            onion_id.set(id);
                            if let Ok(ident) = shared_poll.identity.lock() {
                                if let Ok(uri) = Invite::from_identity(&ident) {
                                    qr_uri.set(uri);
                                }
                            }
                        }
                        OnionEvent::Ready => {
                            let was_down = net_down();
                            net_ready.set(true);
                            net_down.set(false);
                            // Green success banner only after offline→online; auto-hide ~5s.
                            if was_down {
                                ok_banner_visible.set(true);
                                let gen = ok_banner_gen() + 1;
                                ok_banner_gen.set(gen);
                                spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(5)).await;
                                    if ok_banner_gen() == gen {
                                        ok_banner_visible.set(false);
                                    }
                                });
                            }
                        }
                        OnionEvent::Down(s) => {
                            net_ready.set(false);
                            net_down.set(true);
                            dialog_dismissed.set(false);
                            ok_banner_visible.set(false);
                            ok_banner_gen.set(ok_banner_gen() + 1);
                            net_status.set(ui_net_status(&s));
                        }
                        OnionEvent::Error(e) => {
                            net_ready.set(false);
                            net_down.set(true);
                            ok_banner_visible.set(false);
                            ok_banner_gen.set(ok_banner_gen() + 1);
                            net_status.set(ui_net_status(&format!("Error: {e}")));
                        }
                        OnionEvent::DbChanged => {
                            reload_lists(&shared_poll, &mut requests, &mut conversations);
                            match screen() {
                                Screen::Chat { peer_id, title } => {
                                    if let Ok(db) = shared_poll.db.lock() {
                                        if let Ok(msgs) = db.list_messages(&peer_id, 500) {
                                            messages.set(msgs);
                                        }
                                        if let Ok(Some(c)) = db.get_contact(&peer_id) {
                                            let new_title = c.display_name();
                                            if new_title != title {
                                                screen.set(Screen::Chat {
                                                    peer_id: peer_id.clone(),
                                                    title: new_title,
                                                });
                                            }
                                        }
                                    }
                                }
                                Screen::ContactDetail { peer_id, title } => {
                                    if let Ok(db) = shared_poll.db.lock() {
                                        if let Ok(Some(c)) = db.get_contact(&peer_id) {
                                            let new_title = c.display_name();
                                            if new_title != title {
                                                screen.set(Screen::ContactDetail {
                                                    peer_id: peer_id.clone(),
                                                    title: new_title,
                                                });
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                            if let Ok(ident) = shared_poll.identity.lock() {
                                my_nick.set(ident.nick.clone());
                                if let Ok(uri) = Invite::from_identity(&ident) {
                                    qr_uri.set(uri);
                                }
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
    });

    use_context_provider(|| CmdTxContext(cmd_slot.clone()));
    use_context_provider(|| SharedContext(shared.clone()));

    let show_dialog = net_down() && !dialog_dismissed();
    let current = screen();
    let header = current.clone();
    let anim_class = match nav_anim() {
        NavAnim::Forward => "screen-pane enter-forward",
        NavAnim::Back => "screen-pane enter-back",
        NavAnim::None => "screen-pane",
    };
    let pane_key = screen_key(&current);

    rsx! {
        style { {include_str!("style.css")} }

        div { class: "app",
            if net_down() {
                div { class: "banner warn",
                    "Network unavailable — connecting… (auto-retry). Local history available; send disabled."
                }
            } else if ok_banner_visible() {
                div { class: "banner ok", "Connected" }
            }

            if show_dialog {
                div { class: "modal-backdrop",
                    div { class: "modal",
                        h2 { "Network unavailable" }
                        p { "{net_status}" }
                        p { class: "muted",
                            "Retrying automatically. Dismiss to keep reading local history."
                        }
                        button {
                            class: "primary",
                            onclick: move |_| dialog_dismissed.set(true),
                            "Dismiss"
                        }
                    }
                }
            }

            // Header depends on screen
            match header {
                Screen::Chats => rsx! {
                    header { class: "app-bar",
                        button {
                            class: "icon-btn",
                            title: "Account & settings",
                            onclick: {
                                let shared = shared.clone();
                                move |_| {
                                    if let Ok(ident) = shared.identity.lock() {
                                        nick_edit.set(ident.nick.clone());
                                        if let Ok(uri) = Invite::from_identity(&ident) {
                                            qr_uri.set(uri);
                                        }
                                    }
                                    navigate(screen, nav_anim, Screen::Settings, NavAnim::Forward);
                                }
                            },
                            "☰"
                        }
                        div { class: "brand-mark",
                            img {
                                class: "brand-logo",
                                src: brand_logo_src(),
                                alt: "NotChat",
                            }
                            h1 { class: "title", "Chats" }
                        }
                        button {
                            class: "icon-btn",
                            title: "Add contact",
                            onclick: move |_| navigate(screen, nav_anim, Screen::AddContact, NavAnim::Forward),
                            "+"
                        }
                    }
                },
                Screen::Chat { peer_id, title } => rsx! {
                    header { class: "app-bar",
                        button {
                            class: "icon-btn",
                            onclick: {
                                let shared = shared.clone();
                                let peer_id = peer_id.clone();
                                move |_| {
                                    // Persist composer text when leaving the chat.
                                    persist_draft(&shared, &peer_id, &draft());
                                    navigate(screen, nav_anim, Screen::Chats, NavAnim::Back);
                                }
                            },
                            "←"
                        }
                        button {
                            class: "title-btn left",
                            title: "Contact info",
                            onclick: {
                                let peer_id = peer_id.clone();
                                let title = title.clone();
                                let shared = shared.clone();
                                move |_| {
                                    let mut nick = String::new();
                                    if let Ok(db) = shared.db.lock() {
                                        if let Ok(Some(c)) = db.get_contact(&peer_id) {
                                            nick = c.custom_nick.clone().unwrap_or_default();
                                        }
                                    }
                                    custom_nick_edit.set(nick);
                                    persist_draft(&shared, &peer_id, &draft());
                                    navigate(screen, nav_anim, 
                                        Screen::ContactDetail {
                                            peer_id: peer_id.clone(),
                                            title: title.clone(),
                                        },
                                        NavAnim::Forward,
                                    );
                                }
                            },
                            h1 { class: "title left", "{title}" }
                        }
                        button {
                            class: "icon-btn",
                            title: "Contact info",
                            onclick: {
                                let peer_id = peer_id.clone();
                                let title = title.clone();
                                let shared = shared.clone();
                                move |_| {
                                    let mut nick = String::new();
                                    if let Ok(db) = shared.db.lock() {
                                        if let Ok(Some(c)) = db.get_contact(&peer_id) {
                                            nick = c.custom_nick.clone().unwrap_or_default();
                                        }
                                    }
                                    custom_nick_edit.set(nick);
                                    persist_draft(&shared, &peer_id, &draft());
                                    navigate(screen, nav_anim, 
                                        Screen::ContactDetail {
                                            peer_id: peer_id.clone(),
                                            title: title.clone(),
                                        },
                                        NavAnim::Forward,
                                    );
                                }
                            },
                            "⋮"
                        }
                    }
                },
                Screen::ContactDetail { .. } => rsx! {
                    header { class: "app-bar",
                        button {
                            class: "icon-btn",
                            onclick: {
                                let shared = shared.clone();
                                move |_| {
                                    if let Screen::ContactDetail { peer_id, .. } = screen() {
                                        let mut title = peer_id.clone();
                                        if let Ok(db) = shared.db.lock() {
                                            if let Ok(Some(c)) = db.get_contact(&peer_id) {
                                                title = c.display_name();
                                            }
                                        }
                                        navigate(screen, nav_anim, 
                                            Screen::Chat {
                                                peer_id,
                                                title,
                                            },
                                            NavAnim::Back,
                                        );
                                    }
                                }
                            },
                            "←"
                        }
                        h1 { class: "title left", "Contact" }
                        span { style: "width:2.5rem;" }
                    }
                },
                Screen::Settings => rsx! {
                    header { class: "app-bar",
                        button {
                            class: "icon-btn",
                            onclick: move |_| navigate(screen, nav_anim, Screen::Chats, NavAnim::Back),
                            "←"
                        }
                        h1 { class: "title left", "Settings" }
                        span { style: "width:2.5rem;" }
                    }
                },
                Screen::AddContact => rsx! {
                    header { class: "app-bar",
                        button {
                            class: "icon-btn",
                            onclick: move |_| {
                                scanning.set(false);
                                let _ = document::eval("window.__ocQrStop = true;");
                                navigate(screen, nav_anim, Screen::Chats, NavAnim::Back);
                            },
                            "←"
                        }
                        h1 { class: "title left", "Add contact" }
                        span { style: "width:2.5rem;" }
                    }
                },
            }

            if !toast().is_empty() {
                p { class: "toast", "{toast}" }
            }

            div {
                class: "{anim_class}",
                key: "{pane_key}",
                match current {
                    Screen::Chats => rsx! {
                        ChatsView {
                            conversations: conversations(),
                            requests: requests(),
                            screen: screen,
                            nav_anim: nav_anim,
                            messages: messages,
                            draft: draft,
                            custom_nick_edit: custom_nick_edit,
                            toast: toast,
                        }
                    },
                    Screen::Chat { peer_id, title: _ } => rsx! {
                        ChatView {
                            peer_id: peer_id,
                            messages: messages(),
                            draft: draft,
                            net_ready: net_ready(),
                            toast: toast,
                            messages_sig: messages,
                        }
                    },
                    Screen::ContactDetail { peer_id, title } => rsx! {
                        ContactDetailView {
                            peer_id: peer_id,
                            title: title,
                            custom_nick_edit: custom_nick_edit,
                            screen: screen,
                            nav_anim: nav_anim,
                            toast: toast,
                        }
                    },
                    Screen::Settings => rsx! {
                        SettingsView {
                            onion_id: onion_id(),
                            nick_edit: nick_edit,
                            qr_uri: qr_uri(),
                            net_status: net_status(),
                            net_ready: net_ready(),
                            my_nick: my_nick,
                            toast: toast,
                        }
                    },
                    Screen::AddContact => rsx! {
                        AddContactView {
                            add_field: add_field,
                            net_ready: net_ready(),
                            scanning: scanning,
                            screen: screen,
                            nav_anim: nav_anim,
                            toast: toast,
                        }
                    },
                }
            }
        }
    }
}

#[component]
fn ChatsView(
    conversations: Vec<(Contact, Option<Message>)>,
    requests: Vec<ContactRequest>,
    mut screen: Signal<Screen>,
    mut nav_anim: Signal<NavAnim>,
    mut messages: Signal<Vec<Message>>,
    mut draft: Signal<String>,
    mut custom_nick_edit: Signal<String>,
    mut toast: Signal<String>,
) -> Element {
    let shared = use_context::<SharedContext>().0;
    let cmd_slot = use_context::<CmdTxContext>().0;
    let empty = conversations.is_empty() && requests.is_empty();

    rsx! {
        if empty {
            div { class: "empty-state",
                h3 { "No chats yet" }
                p { "Tap + to scan a QR or paste an invite." }
            }
        } else {
            ul { class: "chats-list",
                for r in requests {
                    {
                        let rowid = r.rowid;
                        let label = if r.from_nick.is_empty() {
                            format!("{}…", &r.from_id[..8.min(r.from_id.len())])
                        } else {
                            r.from_nick.clone()
                        };
                        let time = relative_time(r.ts);
                        let cmd_a = cmd_slot.clone();
                        let cmd_b = cmd_slot.clone();
                        rsx! {
                            li {
                                div { class: "chat-row request-row",
                                    div { class: "req-head",
                                        div { class: "row-main",
                                            div { class: "name", "{label}" }
                                            div { class: "req-label", "Contact request" }
                                        }
                                        span { class: "time", "{time}" }
                                    }
                                    div { class: "req-actions",
                                        button {
                                            class: "success-btn",
                                            onclick: move |_| {
                                                dispatch(&cmd_a, OnionCommand::RespondRequest {
                                                    rowid,
                                                    accept: true,
                                                });
                                                toast.set("Accepted.".into());
                                            },
                                            "Accept"
                                        }
                                        button {
                                            class: "danger",
                                            onclick: move |_| {
                                                dispatch(&cmd_b, OnionCommand::RespondRequest {
                                                    rowid,
                                                    accept: false,
                                                });
                                                toast.set("Declined.".into());
                                            },
                                            "Decline"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                for (c, last) in conversations {
                    {
                        let peer = c.id.clone();
                        let title = c.display_name();
                        let title2 = title.clone();
                        let preview = match &last {
                            Some(m) => preview_text(m),
                            None => "No messages yet".to_string(),
                        };
                        let time = match &last {
                            Some(m) => relative_time(m.ts),
                            None => relative_time(c.updated_at),
                        };
                        let shared = shared.clone();
                        rsx! {
                            li {
                                button {
                                    class: "chat-row",
                                    onclick: move |_| {
                                        if let Ok(db) = shared.db.lock() {
                                            if let Ok(msgs) = db.list_messages(&peer, 500) {
                                                messages.set(msgs);
                                            }
                                        }
                                        draft.set(load_draft(&shared, &peer));
                                        custom_nick_edit.set(String::new());
                                        nav_anim.set(NavAnim::Forward);
                                        screen.set(Screen::Chat {
                                            peer_id: peer.clone(),
                                            title: title2.clone(),
                                        });
                                    },
                                    div { class: "row-main",
                                        div { class: "name", "{title}" }
                                        div { class: "preview", "{preview}" }
                                    }
                                    span { class: "time", "{time}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ChatView(
    peer_id: String,
    messages: Vec<Message>,
    mut draft: Signal<String>,
    net_ready: bool,
    mut toast: Signal<String>,
    mut messages_sig: Signal<Vec<Message>>,
) -> Element {
    let cmd_slot = use_context::<CmdTxContext>().0;
    let shared = use_context::<SharedContext>().0;
    let peer_send = peer_id.clone();
    let cmd_send = cmd_slot.clone();

    rsx! {
        div { class: "chat-screen",
            div { class: "thread",
                for m in messages {
                    {
                        let time = relative_time(m.ts);
                        let body = m.plaintext.clone();
                        let out = m.direction == "out";
                        let ticks = if out {
                            outbound_ticks(&m.status)
                        } else {
                            ""
                        };
                        let tick_class = if out && (m.status == "sent" || m.status == "delivered") {
                            "ticks sent"
                        } else {
                            "ticks"
                        };
                        rsx! {
                            div {
                                class: if out { "bubble out" } else { "bubble in" },
                                p { "{body}" }
                                span { class: "meta",
                                    "{time}"
                                    if out {
                                        span { class: "{tick_class}", "{ticks}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            div { class: "composer",
                input {
                    placeholder: if net_ready { "Message…" } else { "Send disabled (network unavailable)" },
                    value: "{draft}",
                    disabled: !net_ready,
                    oninput: {
                        let shared = shared.clone();
                        let peer_draft = peer_id.clone();
                        move |e| {
                            let v = e.value();
                            draft.set(v.clone());
                            persist_draft(&shared, &peer_draft, &v);
                        }
                    },
                }
                button {
                    class: "send-btn",
                    disabled: !net_ready || draft().trim().is_empty(),
                    onclick: move |_| {
                        let text = draft().trim().to_string();
                        if text.is_empty() { return; }
                        // Show in-thread immediately (single tick). DbChanged replaces with
                        // the persisted row (same text; ticks update on delivery).
                        // No "Message queued" toast.
                        let ts = now_unix_ms();
                        let mut list = messages_sig();
                        list.push(Message {
                            msg_id: format!("local-{ts}"),
                            peer_id: peer_send.clone(),
                            direction: "out".into(),
                            plaintext: text.clone(),
                            ts,
                            status: "queued".into(),
                        });
                        messages_sig.set(list);
                        dispatch(&cmd_send, OnionCommand::SendMessage {
                            peer_id: peer_send.clone(),
                            text,
                        });
                        draft.set(String::new());
                        persist_draft(&shared, &peer_send, "");
                        let _ = &toast;
                    },
                    "➤"
                }
            }
        }
    }
}

#[component]
fn ContactDetailView(
    peer_id: String,
    title: String,
    mut custom_nick_edit: Signal<String>,
    mut screen: Signal<Screen>,
    mut nav_anim: Signal<NavAnim>,
    mut toast: Signal<String>,
) -> Element {
    let cmd_slot = use_context::<CmdTxContext>().0;
    let shared = use_context::<SharedContext>().0;
    let peer_save = peer_id.clone();
    let id_display = peer_id.clone();
    let _title = title;

    rsx! {
        div { class: "settings contact-detail",
            div { class: "card",
                h2 { "Contact" }
                p { class: "label", "Nickname (local only)" }
                div { class: "row",
                    input {
                        class: "grow",
                        placeholder: "Custom nickname",
                        value: "{custom_nick_edit}",
                        oninput: move |e| custom_nick_edit.set(e.value()),
                    }
                    button {
                        class: "primary",
                        onclick: {
                            let cmd = cmd_slot.clone();
                            let shared = shared.clone();
                            move |_| {
                                let nick = custom_nick_edit();
                                dispatch(&cmd, OnionCommand::SetCustomNick {
                                    peer_id: peer_save.clone(),
                                    nick: if nick.is_empty() { None } else { Some(nick) },
                                });
                                let mut new_title = peer_save.clone();
                                if let Ok(db) = shared.db.lock() {
                                    if let Ok(Some(c)) = db.get_contact(&peer_save) {
                                        // Prefer the nick we just set for immediate feedback.
                                        let edited = custom_nick_edit();
                                        new_title = if edited.is_empty() {
                                            c.display_name()
                                        } else {
                                            edited
                                        };
                                    }
                                } else {
                                    let edited = custom_nick_edit();
                                    if !edited.is_empty() {
                                        new_title = edited;
                                    }
                                }
                                toast.set("Nickname saved.".into());
                                nav_anim.set(NavAnim::Back);
                                screen.set(Screen::Chat {
                                    peer_id: peer_save.clone(),
                                    title: new_title,
                                });
                            }
                        },
                        "Save"
                    }
                }
                p { class: "label", "ID (56-char)" }
                p { class: "mono id", "{id_display}" }
                p { class: "muted small", "Opaque peer id — not shared as a hostname." }
                div { class: "row",
                    button {
                        class: "secondary",
                        onclick: {
                            let shared = shared.clone();
                            move |_| {
                                let mut new_title = peer_id.clone();
                                if let Ok(db) = shared.db.lock() {
                                    if let Ok(Some(c)) = db.get_contact(&peer_id) {
                                        new_title = c.display_name();
                                    }
                                }
                                nav_anim.set(NavAnim::Back);
                                screen.set(Screen::Chat {
                                    peer_id: peer_id.clone(),
                                    title: new_title,
                                });
                            }
                        },
                        "Back"
                    }
                }
            }
        }
    }
}

#[component]
fn SettingsView(
    onion_id: String,
    mut nick_edit: Signal<String>,
    qr_uri: String,
    net_status: String,
    net_ready: bool,
    mut my_nick: Signal<String>,
    mut toast: Signal<String>,
) -> Element {
    let cmd_slot = use_context::<CmdTxContext>().0;
    let uri_copy = qr_uri.clone();
    let qr_img = if qr_uri.is_empty() {
        None
    } else {
        invite_qr_data_uri(&qr_uri).ok()
    };
    let status_class = if net_ready { "status-pill online" } else { "status-pill offline" };
    let status_label = if net_ready { "Online" } else { "Offline" };

    // Android background/notification status (None on desktop). Re-read every
    // 2s while Settings is open so it reflects permission dialogs.
    let mut bg_tick = use_signal(|| 0u32);
    use_future(move || async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            bg_tick.set(bg_tick().wrapping_add(1));
        }
    });
    let _ = bg_tick();
    let bg_status = crate::notify::background_status();

    rsx! {
        div { class: "settings",
            div { class: "settings-brand",
                img {
                    src: brand_logo_large_src(),
                    alt: "NotChat",
                }
                div {
                    h2 { class: "brand-name", "NotChat" }
                    p { class: "brand-tag", "Private peer-to-peer chat" }
                }
            }
            div { class: "card",
                h2 { "Account" }
                p { class: "label", "Network status" }
                div { class: "{status_class}",
                    span { class: "status-dot" }
                    "{status_label} — {net_status}"
                }

                p { class: "label", "Your ID (56-char)" }
                p { class: "mono id",
                    if onion_id.is_empty() {
                        "— waiting —"
                    } else {
                        "{onion_id}"
                    }
                }

                p { class: "label", "Display nick (shared on intro)" }
                div { class: "row",
                    input {
                        class: "grow",
                        value: "{nick_edit}",
                        oninput: move |e| nick_edit.set(e.value()),
                    }
                    button {
                        class: "primary",
                        onclick: move |_| {
                            let nick = nick_edit();
                            dispatch(&cmd_slot, OnionCommand::SetNick { nick: nick.clone() });
                            my_nick.set(nick);
                            toast.set("Nick saved.".into());
                        },
                        "Save"
                    }
                }
            }

            if let Some(bg) = bg_status {
                div { class: "card",
                    h2 { "Background & notifications" }
                    p { class: "muted small bg-status", "{bg}" }
                    p { class: "muted small",
                        "NotChat keeps a low-priority \"NotChat is running\" notification so it can receive and retry messages while in the background. Some phones (Samsung, Xiaomi, …) still kill background apps unless battery use is set to Unrestricted."
                    }
                    div { class: "row",
                        button {
                            class: "secondary",
                            onclick: move |_| {
                                if !crate::notify::request_notification_permission() {
                                    toast.set("Could not open notification settings.".into());
                                }
                                bg_tick.set(bg_tick().wrapping_add(1));
                            },
                            "Notifications…"
                        }
                        button {
                            class: "secondary",
                            onclick: move |_| {
                                if !crate::notify::request_battery_exemption() {
                                    toast.set("Could not open battery settings.".into());
                                }
                                bg_tick.set(bg_tick().wrapping_add(1));
                            },
                            "Battery: unrestricted…"
                        }
                    }
                }
            }

            div { class: "card",
                h2 { "Invite QR" }
                if let Some(src) = qr_img {
                    div { class: "qr-wrap",
                        img {
                            class: "qr-img",
                            src: "{src}",
                            alt: "Invite QR",
                        }
                    }
                    p { class: "muted small", "Show this QR so another phone can scan it." }
                } else {
                    p { class: "muted", "QR available once your ID is ready." }
                }

                p { class: "label", "Invite URI" }
                textarea {
                    class: "qr",
                    readonly: true,
                    value: "{qr_uri}",
                }
                div { class: "row",
                    button {
                        class: "secondary",
                        disabled: uri_copy.is_empty(),
                        onclick: move |_| {
                            #[cfg(feature = "desktop")]
                            {
                                match arboard::Clipboard::new().and_then(|mut c| c.set_text(uri_copy.clone())) {
                                    Ok(()) => toast.set("Invite URI copied.".into()),
                                    Err(e) => toast.set(format!("Clipboard: {e}")),
                                }
                            }
                            #[cfg(not(feature = "desktop"))]
                            {
                                toast.set("Select the URI text to copy.".into());
                            }
                        },
                        "Copy invite"
                    }
                }
            }
        }
    }
}

#[component]
fn AddContactView(
    mut add_field: Signal<String>,
    net_ready: bool,
    mut scanning: Signal<bool>,
    mut screen: Signal<Screen>,
    mut nav_anim: Signal<NavAnim>,
    mut toast: Signal<String>,
) -> Element {
    let cmd_slot = use_context::<CmdTxContext>().0;
    let cmd_scan = cmd_slot.clone();
    let cmd_file = cmd_slot.clone();
    let cmd_paste = cmd_slot.clone();

    rsx! {
        div { class: "add-screen",
            div { class: "card",
                h2 { "Add contact" }
                p { class: "muted small",
                    "Scan a QR or paste a full "
                    code { "onionchat:v1?…" }
                    " invite. Bare 56-char ID is TOFU-only and cannot send INTRO."
                }

                if scanning() {
                    div { class: "scanner",
                        p { class: "label", "Point camera at invite QR" }
                        video {
                            id: "oc-qr-video",
                            class: "qr-video",
                            autoplay: true,
                            playsinline: true,
                            muted: true,
                        }
                        div { class: "row",
                            button {
                                class: "secondary",
                                onclick: move |_| {
                                    scanning.set(false);
                                    let _ = document::eval("window.__ocQrStop = true;");
                                },
                                "Cancel scan"
                            }
                        }
                        p { class: "muted small", "Only onionchat:v1 payloads are accepted." }
                    }
                }

                div { class: "row",
                    button {
                        class: "primary",
                        disabled: scanning(),
                        onclick: move |_| {
                            scanning.set(true);
                            let cmd = cmd_scan.clone();
                            spawn(async move {
                                tokio::time::sleep(Duration::from_millis(80)).await;
                                let script = build_camera_scan_script();
                                let mut eval = document::eval(&script);
                                match eval.recv::<String>().await {
                                    Ok(raw) => {
                                        if raw.starts_with("ERR:") {
                                            let msg = raw.trim_start_matches("ERR:").trim();
                                            if msg != "cancelled" {
                                                toast.set(format!("Scan: {msg}"));
                                            }
                                            scanning.set(false);
                                            let _ = document::eval("window.__ocQrStop = true;");
                                        } else {
                                            apply_scanned_invite(
                                                &raw,
                                                &cmd,
                                                &mut add_field,
                                                &mut scanning,
                                                &mut screen,
                                                &mut nav_anim,
                                                &mut toast,
                                            );
                                        }
                                    }
                                    Err(e) => {
                                        toast.set(format!("Scan failed: {e}"));
                                        scanning.set(false);
                                        let _ = document::eval("window.__ocQrStop = true;");
                                    }
                                }
                            });
                        },
                        "Scan QR"
                    }
                    button {
                        class: "secondary",
                        disabled: scanning(),
                        onclick: move |_| {
                            let cmd = cmd_file.clone();
                            spawn(async move {
                                let pick = document::eval(
                                    r#"
                                    const input = document.getElementById("oc-qr-file");
                                    if (!input) { dioxus.send("ERR:file input missing"); return; }
                                    input.value = "";
                                    await new Promise((resolve) => {
                                      const done = () => { input.removeEventListener("change", done); resolve(); };
                                      input.addEventListener("change", done);
                                      input.click();
                                    });
                                    if (!input.files || !input.files[0]) {
                                      dioxus.send("ERR:cancelled");
                                      return;
                                    }
                                    dioxus.send("READY");
                                    "#,
                                );
                                let mut pick = pick;
                                match pick.recv::<String>().await {
                                    Ok(s) if s == "READY" => {
                                        let script = build_file_decode_script();
                                        let mut eval = document::eval(&script);
                                        match eval.recv::<String>().await {
                                            Ok(raw) if raw.starts_with("ERR:") => {
                                                let msg = raw.trim_start_matches("ERR:").trim();
                                                if msg != "cancelled" {
                                                    toast.set(format!("Image QR: {msg}"));
                                                }
                                            }
                                            Ok(raw) => {
                                                apply_scanned_invite(
                                                    &raw,
                                                    &cmd,
                                                    &mut add_field,
                                                    &mut scanning,
                                                    &mut screen,
                                                    &mut nav_anim,
                                                    &mut toast,
                                                );
                                            }
                                            Err(e) => toast.set(format!("Image QR failed: {e}")),
                                        }
                                    }
                                    Ok(s) if s.starts_with("ERR:") => {
                                        let msg = s.trim_start_matches("ERR:").trim();
                                        if msg != "cancelled" {
                                            toast.set(format!("Image: {msg}"));
                                        }
                                    }
                                    Ok(other) => toast.set(format!("Image: {other}")),
                                    Err(e) => toast.set(format!("Image pick failed: {e}")),
                                }
                            });
                        },
                        "Load QR image"
                    }
                }

                canvas {
                    id: "oc-qr-canvas",
                    class: "qr-canvas-hidden",
                }
                input {
                    id: "oc-qr-file",
                    r#type: "file",
                    accept: "image/*",
                    class: "qr-file-hidden",
                }

                p { class: "label", "Paste invite URI" }
                textarea {
                    class: "qr",
                    placeholder: "onionchat:v1?id=…&pk=…&n=…&nick=…",
                    value: "{add_field}",
                    oninput: move |e| add_field.set(e.value()),
                }
                button {
                    class: "primary",
                    disabled: !net_ready || add_field().trim().is_empty() || scanning(),
                    onclick: move |_| {
                        match Invite::parse(&add_field()) {
                            Ok(inv) => {
                                if inv.tofu {
                                    toast.set("TOFU: bare ID needs full QR URI to INTRO.".into());
                                    return;
                                }
                                dispatch(&cmd_paste, OnionCommand::SendIntro { invite: inv });
                                add_field.set(String::new());
                                toast.set("Contact request sent.".into());
                                nav_anim.set(NavAnim::Back);
                                screen.set(Screen::Chats);
                            }
                            Err(e) => toast.set(format!("Invalid invite: {e}")),
                        }
                    },
                    "Send contact request"
                }
            }
        }
    }
}

fn ensure_jsqr_prefix() -> String {
    let mut s = String::from("if (typeof jsQR !== 'function') {\n");
    s.push_str(include_str!("../assets/jsqr.min.js"));
    s.push_str("\n}\n");
    s
}

fn build_camera_scan_script() -> String {
    let mut s = ensure_jsqr_prefix();
    s.push_str(include_str!("../assets/qr_scanner_body.js"));
    s
}

fn build_file_decode_script() -> String {
    let mut s = ensure_jsqr_prefix();
    s.push_str(include_str!("../assets/qr_file_decode_body.js"));
    s
}

fn apply_scanned_invite(
    raw: &str,
    cmd_slot: &CmdTx,
    add_field: &mut Signal<String>,
    scanning: &mut Signal<bool>,
    screen: &mut Signal<Screen>,
    nav_anim: &mut Signal<NavAnim>,
    toast: &mut Signal<String>,
) {
    match Invite::parse_scanned(raw) {
        Ok(inv) => {
            dispatch(cmd_slot, OnionCommand::SendIntro { invite: inv });
            add_field.set(String::new());
            scanning.set(false);
            let _ = document::eval("window.__ocQrStop = true;");
            toast.set("Contact request sent.".into());
            nav_anim.set(NavAnim::Back);
            screen.set(Screen::Chats);
        }
        Err(e) => {
            scanning.set(false);
            let _ = document::eval("window.__ocQrStop = true;");
            toast.set(format!("Invalid QR: {e}"));
        }
    }
}

fn reload_lists(
    shared: &AppShared,
    requests: &mut Signal<Vec<ContactRequest>>,
    conversations: &mut Signal<Vec<(Contact, Option<Message>)>>,
) {
    if let Ok(db) = shared.db.lock() {
        if let Ok(r) = db.list_pending_requests() {
            requests.set(r);
        }
        if let Ok(conv) = db.list_conversations() {
            conversations.set(conv);
        }
    }
}

/// Navigate to a chat because the user tapped its notification.
fn open_chat_from_notification(
    shared: &AppShared,
    peer: &str,
    mut screen: Signal<Screen>,
    mut nav_anim: Signal<NavAnim>,
    mut messages: Signal<Vec<Message>>,
    mut draft: Signal<String>,
    mut custom_nick_edit: Signal<String>,
) {
    let current = screen();
    if let Screen::Chat { peer_id, .. } = &current {
        if peer_id == peer {
            return;
        }
        // Leaving another chat: keep its composer text.
        persist_draft(shared, peer_id, &draft());
    }
    let mut title = None;
    if let Ok(db) = shared.db.lock() {
        match db.get_contact(peer) {
            Ok(Some(c)) if c.status == crate::db::ContactStatus::Accepted => {
                title = Some(c.display_name());
                if let Ok(msgs) = db.list_messages(peer, 500) {
                    messages.set(msgs);
                }
            }
            _ => {}
        }
    }
    let Some(title) = title else {
        // Contact request (not yet accepted) → requests live on the Chats list.
        if current != Screen::Chats {
            nav_anim.set(NavAnim::Back);
            screen.set(Screen::Chats);
        }
        return;
    };
    draft.set(load_draft(shared, peer));
    custom_nick_edit.set(String::new());
    nav_anim.set(NavAnim::Forward);
    screen.set(Screen::Chat {
        peer_id: peer.to_string(),
        title,
    });
}
