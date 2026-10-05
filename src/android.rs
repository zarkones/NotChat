//! Android JNI glue (only compiled for `target_os = "android"`).
//!
//! Java → Rust (called from Kotlin `NotChatBridge`, package `dev.dioxus.main`):
//!   - `nativeInit(Context)`            — cache JavaVM + bridge class (MainActivity/service).
//!   - `nativeStartBackground(Context)` — start the onion/HTTP/outbox runtime
//!     headless (foreground service, boot, periodic backup job). Idempotent: the
//!     UI shares the very same runtime via `crate::runtime`.
//!
//! Rust → Java (static methods on `NotChatBridge`):
//!   - `postMessage(peer, title, body)`, `postContactRequest(peer, title)`
//!   - `setActivePeer(peer?)`, `takePendingOpenPeer(): String?`
//!   - `backgroundStatus(): String`, `requestBatteryExemption(): Boolean`,
//!     `requestNotificationPermission(): Boolean`
//!
//! All calls run inside a JNI local frame (the UI thread is a permanently
//! attached daemon thread, so leaked locals would accumulate), and Java
//! exceptions are logged + cleared.

use std::sync::OnceLock;

use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, JNI_FALSE, JNI_TRUE};
use jni::{JNIEnv, JavaVM};
use tracing::{info, warn};

static VM: OnceLock<JavaVM> = OnceLock::new();
static BRIDGE: OnceLock<GlobalRef> = OnceLock::new();

fn cache(env: &mut JNIEnv, class: &JClass) {
    if VM.get().is_none() {
        if let Ok(vm) = env.get_java_vm() {
            let _ = VM.set(vm);
        }
    }
    if BRIDGE.get().is_none() {
        if let Ok(g) = env.new_global_ref(class) {
            let _ = BRIDGE.set(g);
        }
    }
}

#[no_mangle]
pub extern "system" fn Java_dev_dioxus_main_NotChatBridge_nativeInit(
    mut env: JNIEnv,
    class: JClass,
    _ctx: JObject,
) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cache(&mut env, &class);
    }));
}

#[no_mangle]
pub extern "system" fn Java_dev_dioxus_main_NotChatBridge_nativeStartBackground(
    mut env: JNIEnv,
    class: JClass,
    _ctx: JObject,
) -> jboolean {
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        cache(&mut env, &class);
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("info,onion_chat=info"))
            .try_init();
        let root = crate::identity::default_data_root();
        match crate::runtime::ensure_started(&root) {
            Ok(_) => {
                info!("background runtime running");
                true
            }
            Err(e) => {
                warn!(?e, "background runtime failed to start");
                false
            }
        }
    }));
    match res {
        Ok(true) => JNI_TRUE,
        _ => JNI_FALSE,
    }
}

/// Keep the JNI exports alive even if the linker would otherwise GC them.
#[used]
static KEEP_INIT: extern "system" fn(JNIEnv, JClass, JObject) =
    Java_dev_dioxus_main_NotChatBridge_nativeInit;
#[used]
static KEEP_START: extern "system" fn(JNIEnv, JClass, JObject) -> jboolean =
    Java_dev_dioxus_main_NotChatBridge_nativeStartBackground;

/// Run `f` with an attached env + bridge class inside a local frame.
fn with_bridge<T>(
    f: impl FnOnce(&mut JNIEnv, &JClass) -> jni::errors::Result<T>,
) -> Option<T> {
    let vm = VM.get()?;
    let bridge = BRIDGE.get()?;
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(e) => {
            warn!(?e, "JNI attach failed");
            return None;
        }
    };
    let class: &JClass = <&JClass>::from(bridge.as_obj());
    let out = env.with_local_frame(16, |env| f(env, class));
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
    match out {
        Ok(v) => Some(v),
        Err(e) => {
            warn!(?e, "JNI call failed");
            None
        }
    }
}

pub fn post_message(peer_id: &str, title: &str, body: &str) {
    with_bridge(|env, class| {
        let p = env.new_string(peer_id)?;
        let t = env.new_string(title)?;
        let b = env.new_string(body)?;
        env.call_static_method(
            class,
            "postMessage",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V",
            &[JValue::Object(&p), JValue::Object(&t), JValue::Object(&b)],
        )?;
        Ok(())
    });
}

pub fn post_contact_request(peer_id: &str, title: &str) {
    with_bridge(|env, class| {
        let p = env.new_string(peer_id)?;
        let t = env.new_string(title)?;
        env.call_static_method(
            class,
            "postContactRequest",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[JValue::Object(&p), JValue::Object(&t)],
        )?;
        Ok(())
    });
}

pub fn set_active_peer(peer_id: Option<&str>) {
    with_bridge(|env, class| {
        let p: JObject = match peer_id {
            Some(s) => env.new_string(s)?.into(),
            None => JObject::null(),
        };
        env.call_static_method(
            class,
            "setActivePeer",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&p)],
        )?;
        Ok(())
    });
}

fn call_string(name: &str) -> Option<String> {
    with_bridge(|env, class| {
        let obj = env
            .call_static_method(class, name, "()Ljava/lang/String;", &[])?
            .l()?;
        if obj.is_null() {
            return Ok(None);
        }
        let js = JString::from(obj);
        let s: String = env.get_string(&js)?.into();
        Ok(Some(s))
    })
    .flatten()
}

fn call_bool(name: &str) -> bool {
    with_bridge(|env, class| env.call_static_method(class, name, "()Z", &[])?.z())
        .unwrap_or(false)
}

pub fn take_pending_open_peer() -> Option<String> {
    call_string("takePendingOpenPeer")
}

pub fn background_status() -> Option<String> {
    call_string("backgroundStatus")
}

pub fn request_battery_exemption() -> bool {
    call_bool("requestBatteryExemption")
}

pub fn request_notification_permission() -> bool {
    call_bool("requestNotificationPermission")
}

/// Update the ongoing "NotChat is running" notification text.
pub fn set_service_status(text: &str) {
    with_bridge(|env, class| {
        let t = env.new_string(text)?;
        env.call_static_method(
            class,
            "setServiceStatus",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&t)],
        )?;
        Ok(())
    });
}
