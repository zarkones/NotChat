//! NotChat (onion-chat-v0) — P2P text chat over Tor onion services.
//!
//! Modules: identity, crypto, db, protocol, http, onion, runtime (process
//! singleton), notify (push notifications), android (JNI, android only),
//! ui (feature-gated).

pub mod crypto;
pub mod db;
pub mod http;
pub mod identity;
pub mod notify;
pub mod onion;
pub mod protocol;
pub mod qr_display;
pub mod runtime;

#[cfg(target_os = "android")]
pub mod android;

#[cfg(any(feature = "desktop", feature = "mobile"))]
pub mod ui;

pub use http::AppShared;
pub use identity::default_data_root;
pub use onion::{open_app_state, spawn_onion_thread, OnionCommand, OnionEvent};
