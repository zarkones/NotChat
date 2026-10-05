//! Incoming-message notifications (platform-agnostic front).
//!
//! Rule: post a notification for every inbound message **unless** the user is
//! currently looking at that peer's chat screen.
//!
//! "Looking at" = both of:
//!   1. the Dioxus UI's current screen is `Screen::Chat { peer_id }` for that
//!      peer (mirrored here via [`set_active_chat`] from a `use_effect` in ui.rs), and
//!   2. the Android Activity is resumed (tracked in Kotlin by `MainActivity`
//!      onResume/onPause — screen off / app in background ⇒ not resumed).
//!
//! Rust owns (1) and forwards it to Kotlin; Kotlin owns (2) and makes the final
//! decision in `NotChatBridge.postMessage`, so there is no race with the
//! Activity lifecycle. On desktop/headless this module only logs.
//!
//! Mute: no per-contact mute exists yet; [`is_muted`] is the single hook.

use std::sync::Mutex;

use tracing::info;

static ACTIVE_CHAT: Mutex<Option<String>> = Mutex::new(None);

/// Called by the UI whenever the screen changes. `Some(peer)` while a chat is open.
pub fn set_active_chat(peer_id: Option<&str>) {
    {
        let mut g = ACTIVE_CHAT.lock().unwrap_or_else(|p| p.into_inner());
        if g.as_deref() == peer_id {
            return;
        }
        *g = peer_id.map(str::to_string);
    }
    #[cfg(target_os = "android")]
    crate::android::set_active_peer(peer_id);
}

pub fn active_chat() -> Option<String> {
    ACTIVE_CHAT
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

/// Mute hook. No mute setting exists in v0 → never muted.
pub fn is_muted(_peer_id: &str) -> bool {
    false
}

fn preview(text: &str) -> String {
    let flat = text.replace(['\n', '\r'], " ");
    if flat.chars().count() > 140 {
        let t: String = flat.chars().take(137).collect();
        format!("{t}…")
    } else {
        flat
    }
}

/// Inbound `/v1/msg` was accepted and stored.
pub fn on_incoming_message(peer_id: &str, display_name: &str, text: &str) {
    if is_muted(peer_id) {
        return;
    }
    let body = preview(text);
    info!(peer = %&peer_id[..8.min(peer_id.len())], "incoming message → notify (unless chat open)");
    #[cfg(target_os = "android")]
    crate::android::post_message(peer_id, display_name, &body);
    #[cfg(not(target_os = "android"))]
    {
        let _ = (display_name, body);
    }
}

/// Inbound `/v1/intro` created a new pending contact request.
pub fn on_contact_request(peer_id: &str, nick: &str) {
    let title = if nick.is_empty() {
        format!("{}…", &peer_id[..8.min(peer_id.len())])
    } else {
        nick.to_string()
    };
    #[cfg(target_os = "android")]
    crate::android::post_contact_request(peer_id, &title);
    #[cfg(not(target_os = "android"))]
    {
        let _ = title;
    }
}

/// Peer id from a tapped notification (cold or warm start), consumed once.
pub fn take_pending_open_peer() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        crate::android::take_pending_open_peer()
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

/// Human-readable background/notification status for Settings.
pub fn background_status() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        crate::android::background_status()
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

/// Ask Android to exempt NotChat from battery optimization (system dialog).
pub fn request_battery_exemption() -> bool {
    #[cfg(target_os = "android")]
    {
        crate::android::request_battery_exemption()
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}

/// Re-ask POST_NOTIFICATIONS (Android 13+) / open app notification settings.
pub fn request_notification_permission() -> bool {
    #[cfg(target_os = "android")]
    {
        crate::android::request_notification_permission()
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}
