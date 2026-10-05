# NotChat (onion-chat-v0)

**User-facing name:** NotChat. Crate/package id remains `onion-chat` / `dev.zarkones.onion_chat`.

P2P **text-only** chat over **Tor v3 onion services**. No middle server. Each install hosts a persistent onion; peers connect to each other via HTTP on the onion.

Built on the earlier Arti + Dioxus PoC: identity, crypto, SQLite, contact requests, messaging, and UI are layered on the same crash-safe Arti thread pattern.

**Authority:** see [`SPEC-v0.md`](./SPEC-v0.md) (frozen wire format at the end).

## Threat model

Protect against network observers and identity/location correlation. **Not** against someone with physical access to an unlocked phone.

## What v0 includes

| Area | Behavior |
|------|----------|
| Identity | App-layer Ed25519 keypair (persisted) + Tor onion as transport ID (56-char, no `.onion` in normal UI) |
| Crypto | Signed frames; XChaCha20-Poly1305 sealed text (`crypto_box`); reject bad sig / skew >10m / replayed nonces |
| DB | SQLite: profile, contacts, requests, messages, outbox, seen nonces |
| HTTP | `GET /v1/health`, `POST /v1/intro`, `POST /v1/intro/ack`, `POST /v1/msg` on onion port 80 |
| Outbound | Arti `TorClient` POSTs to peer onions; outbox + retry when unreachable |
| UI | Tor banner + dismissible dialog; contacts; requests; chat; profile (invite QR image + URI); add via Scan QR / paste / load image |
| Always-on | Onion starts on launch; auto-retry forever while process lives |

**Non-goals:** groups, media, voice, cloud backup, username directory, multi-device sync.

## Contact flow (QR → INTRO → ACK → MSG)

```text
Alice shows QR / invite URI from Profile
  onionchat:v1?id=<56char>&pk=<b64url>&n=<b64url>&nick=<urlenc>

Bob scans Alice's QR (or pastes URI) on Add → app queues signed POST /v1/intro to Alice's onion
  (to_nonce must match Alice's current invite nonce)

Alice sees Contact request → Accept
  → local status=accepted
  → queues signed POST /v1/intro/ack { decision: "accept" } to Bob

Both can now POST /v1/msg (sealed UTF-8 text). Reject drops the request.
Bare 56-char ID paste is TOFU-only and cannot send INTRO (needs full QR with pk+nonce).
```

Never show `.onion` in chats/contacts. Profile shows the **56-character ID**. Nicknames are local labels (custom nick → else peer self-nick).

## Layout

| Path | Role |
|------|------|
| `SPEC-v0.md` | Product + frozen wire format |
| `src/identity.rs` | Ed25519 identity, onion id helpers, data root |
| `src/crypto.rs` | Sign / verify / seal / skew |
| `src/db.rs` | SQLite schema + queries |
| `src/protocol.rs` | QR URI + JSON frames |
| `src/qr_display.rs` | Invite → SVG QR data-URI (`qrcode` crate) |
| `assets/` | Vendored `jsqr.min.js` + scanner JS for WebView camera |
| `android/AndroidManifest.xml` | INTERNET + CAMERA for dx Android builds |
| `src/http.rs` | HTTP parse + `/v1/*` handlers |
| `src/onion.rs` | Arti thread, auto-retry, outbox, outbound client |
| `src/ui.rs` | Dioxus screens (`desktop` / `mobile`) |
| `src/bin/headless.rs` | Debug CLI (init / qr / demo-crypto / onion) |
| `data/` | Arti state + `onion-chat.sqlite3` (mode 0700) |

## Prerequisites

- **Rust 1.92+** (toolchain pin: `rust-toolchain.toml` → 1.99)
- Network access for Tor bootstrap
- Headless: OpenSSL + pkg-config (this box ships a tiny `.tools/pkg-config` shim)
- Desktop UI: WebKitGTK (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, …)
- Android: SDK + NDK + [`dx`](https://dioxuslabs.com) — see [ANDROID.md](./ANDROID.md)

## Build & run

```bash
cd /workspace/onion-chat-v0   # or your checkout
export PATH="$HOME/.cargo/bin:$(pwd)/.tools:$PATH"

# Library + protocol (no GUI)
cargo build --bin headless
cargo test --lib

# Debug helpers
cargo run --bin headless -- init          # print identity (needs onion id for QR)
cargo run --bin headless -- demo-crypto   # sign + seal self-test
cargo run --bin headless                  # start onion + /v1 HTTP (auto-retry)

# Desktop UI (needs WebKitGTK)
cargo run --features desktop --bin onion-chat
```

Headless onion serves concurrently with descriptor publish (does **not** wait for `fully_reachable` before accepting streams). Soft-ready after ~30s; full ready when Arti reports reachable.

Verify health (from a Tor client):

```bash
torsocks curl -sS http://<56chars>.onion/v1/health
# {"ok":true,"ver":1}
```

## Android APK (on your machine)

```bash
# After Android Studio SDK/NDK + rustup targets + dx CLI:
cd /path/to/onion-chat-v0
dx build --platform android --features mobile --target aarch64-linux-android
# or:
dx build --platform android --features mobile
dx bundle --platform android --features mobile
adb install -r "$(find target -name '*.apk' | head -1)"
```

Package id: `dev.zarkones.onion_chat`. Manifest permissions (`INTERNET`, `CAMERA`) via `Dioxus.toml` + `android/AndroidManifest.xml` — details in [ANDROID.md](./ANDROID.md). On first Scan QR, Android will prompt for camera access.

### Always-on next steps (Android)

In-app today: onion starts on launch, dedicated Arti OS thread + Tokio runtime, forever auto-retry, dismissible Tor-down dialog + persistent banner, send disabled while down.

Still needed for true always-on when the app process is killed / phone reboots (Dioxus does not yet ship this glue):

1. **Foreground service** holding the Tor thread (OEM battery exemptions / “unrestricted” data).
2. **`BOOT_COMPLETED`** (and `QUICKBOOT_POWERON` where relevant) to restart the service.
3. Persist data under app-private storage (already defaulting to `/data/data/dev.zarkones.onion_chat/files/…` on Android).

## Engineering notes

- Arti never runs on the UI/JNI thread — `spawn_onion_thread` owns a multi-thread Tokio runtime; UI uses `mpsc` channels.
- Rend request handling starts immediately alongside status watching (avoids “onion disconnected” while publishing).
- Outbound frames land in SQLite `outbox` and are retried every ~15s while Tor is up.
- rustls `ring` provider installed once at session start; Arti TLS uses `native-tls`.

## Known gaps

- Camera QR scan uses WebView `getUserMedia` + native `BarcodeDetector` with vendored **jsQR** fallback; desktop also has **Load QR image**. Paste still works.
- Desktop UI not compiled on this box (no WebKitGTK / glib); `headless` + `cargo test --lib` verified.
- Android APK / foreground-service / boot receiver not built here (no SDK/NDK).
- No delivery receipts beyond local outbox removal on HTTP 2xx.
- Invite nonce is stable until rotated (v0 does not auto-rotate after each accept).
- Tor reachability still depends on network/NAT; first bootstrap 30–120s typical.

## Verified (2026-10-05, Europe/San_Marino)

| Check | Result |
|-------|--------|
| `cargo build --bin headless` | OK |
| `cargo test --lib` (10 tests) | OK |
| `headless demo-crypto` | OK |
| `cargo check --features desktop` | OK (after WebKitGTK + system pkg-config) |
| `cargo check --features mobile` | OK |
| Android `dx build` | Not attempted (no SDK/NDK here) |

## License

MIT OR Apache-2.0
