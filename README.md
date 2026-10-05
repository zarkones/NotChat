# NotChat

**Private peer-to-peer text chat.** No accounts, no phone numbers, no central server that can read your messages or map your social graph. Each phone runs its own Tor onion service; you add people by scanning a QR (or pasting an invite), then chat over encrypted messages end to end.

Use NotChat when you want messaging that stays offline-friendly to big platforms: metadata stays off their servers, contacts are only people you deliberately added, and the app keeps trying to deliver even when peers are briefly unreachable.

### Why people use it
- **No middleman** — messages go peer to peer over Tor, not through NotChat’s (or anyone’s) cloud
- **No signup** — identity lives on your device; invite someone with a QR
- **Text that seals** — messages are signed and encrypted; bad signatures and replays are dropped
- **Works in the background** — Android foreground service + retries so inbound and queued sends keep moving
- **Simple UI** — Signal-style chat list, drafts, delivery ticks, notifications when you’re not already in that chat

### Features (v0)
- 1:1 text chats with local nicknames
- QR / invite link contact requests (Accept / Decline)
- Persistent drafts per chat
- Message notifications (skipped while that chat is open in the foreground)
- Outbox with automatic retry
- Always-on network keep-alive on Android (battery Unrestricted recommended on some OEMs)
- Open-source; signed release APKs via GitHub Releases

**Not in v0:** groups, media, voice, cloud backup, usernames directory, multi-device sync.

Threat model: network observers and location/identity correlation — **not** someone with an unlocked phone in hand. Details: [`SPEC-v0.md`](./SPEC-v0.md).

---

## Releases

Signed arm64 APKs are attached to [GitHub Releases](https://github.com/zarkones/NotChat/releases) as **`NotChat-arm64.apk`**.

Creating a release (or pushing a `v*` tag) runs [`.github/workflows/android-release.yml`](.github/workflows/android-release.yml). Required repo secrets and local build steps: [ANDROID.md](./ANDROID.md).

```bash
# Latest download (after first release exists)
# https://github.com/zarkones/NotChat/releases/latest/download/NotChat-arm64.apk
```

Local production build:

```bash
android/build-release-apk.sh
# -> ./app-release-arm64.apk
ADB_SERIAL=<device> android/build-release-apk.sh   # optional install
```

Debug build: `android/build-debug-apk.sh`.

---

## How contact works

```text
You show a QR / invite from Settings
  onionchat:v1?id=<56char>&pk=<b64url>&n=<b64url>&nick=<urlenc>

Friend scans or pastes → signed contact request
You Accept → both can send sealed text messages
```

Normal UI shows a **56-character ID**, not a raw onion hostname. Nicknames are local labels.

---

## Build from source

**Prerequisites:** Rust (see `rust-toolchain.toml`), network for Tor bootstrap. Desktop UI needs WebKitGTK. Android needs SDK/NDK + [`dx`](https://dioxuslabs.com) — see [ANDROID.md](./ANDROID.md).

```bash
cd NotChat   # or your checkout path
export PATH="$HOME/.cargo/bin:$PATH"

cargo build --bin headless
cargo test --lib
cargo run --bin headless                  # onion + HTTP (no GUI)
cargo run --features desktop --bin onion-chat   # desktop UI
```

Package id: `dev.zarkones.onion_chat` (crate/bin name `onion-chat` for historical reasons; **app name is NotChat**).

---

## Layout

| Path | Role |
|------|------|
| `SPEC-v0.md` | Product + wire format |
| `src/` | Identity, crypto, DB, protocol, HTTP, Tor, UI, Android notify/runtime |
| `android/` | Manifest overlay, build scripts, launcher icons |
| `assets/` | Logo, QR scanner JS |
| `.github/workflows/` | Release APK CI |

## License

MIT OR Apache-2.0
