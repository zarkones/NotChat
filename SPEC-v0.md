# NotChat — v0 Spec (onion-chat)

## Goal
P2P text chat over Tor onion services. No middle server. Phone hosts a persistent onion; peers connect to each other.

## Threat model
Protect against network observers and identity/location correlation. **Not** against someone with physical access to an unlocked phone.

## Non-goals (v0)
Groups, media, voice, cloud backup, username directory, multi-device sync.

## Identity & UX
- Each install has a stable Tor v3 onion (+ app-layer Ed25519 identity key for auth).
- **Never show `.onion` in normal UI.** In profile details, show the **56-character public ID** (onion hostname without `.onion`). Normies treat it as an opaque ID.
- **Nicknames are local labels**, not global identity:
  - You can set *your* display nick (shared in intros/profile updates).
  - Contacts can set a **custom nick** for someone; if unset, show that peer’s **latest self-nick**.
- Contact list / chat UI: nick (or custom) only — no onion strings.

## Bootstrap / always-on
- Onion service starts on boot and stays up (foreground service; dig in on OEM battery as needed).
- If Tor/onion is down: auto-retry; **blocking dialog** with status (dismissible).
- After dismiss: **top-of-app banner** still shows the problem.
- User can always read local history while offline; **send** disabled until onion is up.

## Adding contacts (QR)
QR (and optional manual paste of 56-char ID) encodes enough for intro, e.g.:
`onionchat:v1?id=<56char>&pk=<identity_pubkey>&n=<…>&nick=<urlenc>`
(optional self-nick in QR or only in INTRO body)

**Display:** Profile renders the invite URI as an SVG QR (`qrcode` crate) so one device can show it.

**Scan:** Add → **Scan QR** opens the camera (WebView `getUserMedia`; Android needs `CAMERA`). Decoded payloads must be `onionchat:v1` with `id`+`pk`+`n` — garbage / other schemes rejected. Desktop can also **Load QR image**. Paste of the URI remains supported.

**Flow:** You scan them → app sends signed **INTRO** to their onion → they get a **contact request** (not auto-chat). Accept → both can message. Reject/ignore → drop.

## Trust
- INTRO authenticated with scanner’s identity key; includes their nonce (anti-replay).
- Accept = trust for messaging. No mutual QR required if they tap Accept (mutual QR optional later).

## Transport
- **HTTP** on the onion (port 80), JSON bodies.
- Suggested paths: `POST /v1/intro`, `POST /v1/intro/ack`, `POST /v1/msg`, `GET /v1/health` (optional).

## Messages
- **Text only.**
- Signed + sealed to peer identity pubkey.
- Local **SQLite** (or equivalent): contacts, requests, keys, message history.
- Offline: queue outbound until peer onion reachable (best-effort); no middle relay.

## Crypto (v0)
- Identity: Ed25519 (app-layer; separate from Tor keys).
- Sealed text messages (e.g. crypto_box / equivalent).
- Tor = transport anonymity; app keys = “this contact is who I accepted.”

## Persistence
Local DB only for history, contacts, nicknames, pending requests, outbox.

## Wire format (frozen)

### QR / manual ID
`onionchat:v1?id=<56char>&pk=<base64url_ed25519_pk>&n=<base64url_16B_nonce>&nick=<urlenc>`

`id` is onion hostname without `.onion`. Manual entry: paste `id` only → TOFU warning until INTRO completes with matching `pk`.

### Crypto
- Ed25519 identity keypair (app-layer).
- X25519 + XChaCha20-Poly1305 sealed boxes for message bodies (or libsodium `crypto_box_easy` equivalent).
- Every frame: `sig` over canonical bytes `ver || type || ts || body`; reject bad sig / skew > 10m / replayed nonce.

### HTTP JSON
`POST /v1/intro` body:
`{ "ver":1, "type":"intro", "ts": <unix_ms>, "from_id":"…", "from_pk":"…", "from_nick":"…", "to_nonce":"…", "sig":"…" }`

`POST /v1/intro/ack`:
`{ "ver":1, "type":"intro_ack", "ts":…, "from_id":"…", "from_pk":"…", "decision":"accept"|"reject", "sig":"…" }`

`POST /v1/msg`:
`{ "ver":1, "type":"msg", "ts":…, "from_id":"…", "msg_id":"<uuid>", "ciphertext":"…", "sig":"…" }`
Ciphertext seals UTF-8 text to peer identity.

`GET /v1/health` → `{ "ok": true, "ver": 1 }`

Unknown contacts without valid intro → drop. Accept required before `/v1/msg`.

## Build note (Arti features)
arti-client must enable both `onion-service-service` (host) and `onion-service-client` (dial peer .onion); without the latter INTRO/outbox never leaves the device.
