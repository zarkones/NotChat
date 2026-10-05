# Build an Android APK (NotChat / onion-chat-v0)

Project: `onion-chat` — package id `dev.zarkones.onion_chat`.

## 0. Caveat

Headless Arti + protocol **already work on Linux**. Android adds cross-compile risk (OpenSSL / SQLite / NDK) and lifecycle risk (Doze killing long-lived onions). In-app auto-retry is implemented; **foreground service + BOOT_COMPLETED** are documented next steps below.

## 1. Tooling (one-time)

### Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustup default stable
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android
```

### Dioxus CLI

```bash
curl -fsSL https://dioxuslabs.com/install.sh | bash
# or: cargo install dioxus-cli --locked
dx --version
```

### Android Studio

Install SDK, Command-line Tools, NDK (side-by-side), CMake. Create an AVD or enable USB debugging.

```bash
export JAVA_HOME="${JAVA_HOME:-/usr/lib/jvm/java-17-openjdk-amd64}"
export ANDROID_HOME="$HOME/Android/Sdk"
export NDK_HOME="$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)"
export PATH="$PATH:$ANDROID_HOME/emulator:$ANDROID_HOME/platform-tools:$ANDROID_HOME/cmdline-tools/latest/bin:$HOME/.cargo/bin"
```

## 2. Build

```bash
cd /path/to/onion-chat-v0
source "$HOME/.cargo/env"

dx build --platform android --features mobile --target aarch64-linux-android

# alternatives
dx build --platform android --features mobile
dx build --platform android --release --features mobile
dx bundle --platform android --features mobile
```

If `dx` ignores `--features`:

```bash
dx build --platform android -- --features mobile
```

Find the APK:

```bash
find target -name '*.apk' 2>/dev/null
adb install -r path/to/app.apk
```

## 3. Permissions (dx custom manifest)

Dioxus 0.7+ reads permissions from `Dioxus.toml`:

```toml
[permissions]
camera = { description = "Scan NotChat invite QR codes" }

[android]
manifest = "android/AndroidManifest.xml"

[android.permissions]
"android.permission.INTERNET" = { description = "Network connectivity for messaging" }
"android.permission.CAMERA" = { description = "Scan NotChat invite QR codes" }
```

### How dx custom manifest works

1. **Unified `[permissions]`** — CLI maps `camera` → Android `CAMERA` (+ iOS usage string).
2. **`[android.permissions]`** — extra Android permission names not covered by the unified table (e.g. `INTERNET`).
3. **`[android] manifest = "…"`** — path to a full/custom `AndroidManifest.xml` that `dx` merges or substitutes into the generated Android project (same role as older `[application] android_manifest`). Keep the activity/`lib_name` scaffolding aligned with the dx template; change permissions/features as needed.

This repo ships `android/AndroidManifest.xml` with:

```xml
<uses-permission android:name="android.permission.INTERNET" />
<uses-permission android:name="android.permission.CAMERA" />
```

`CAMERA` is a **dangerous** permission: the OS prompts at runtime the first time Scan QR calls `getUserMedia`. If the user denies it, use **Load QR image** or paste.

### WebView camera note

Scan QR uses the Dioxus **WebView** (`getUserMedia` + `BarcodeDetector` / vendored jsQR). Android also needs the WebView to grant `RESOURCE_VIDEO_CAPTURE` via `WebChromeClient.onPermissionRequest`. Newer dx/wry builds often do this when `CAMERA` is declared; if Scan QR still fails with a permission / NotAllowed error after allowing the OS prompt:

1. Prefer **Load QR image** or paste (always works).
2. Or eject/customize `MainActivity` (legacy `android_main_activity` / dx template) to `request.grant` video-capture in `onPermissionRequest`.

Desktop Linux WebKit similarly may deny camera until a permission handler exists — use **Load QR image** there.

For always-on (next step), also plan:

```xml
<uses-permission android:name="android.permission.FOREGROUND_SERVICE" />
<uses-permission android:name="android.permission.FOREGROUND_SERVICE_CONNECTED_DEVICE" />
<uses-permission android:name="android.permission.RECEIVE_BOOT_COMPLETED" />
<uses-permission android:name="android.permission.POST_NOTIFICATIONS" />
```

Wire a small Kotlin/Java `Service` + `BroadcastReceiver` that starts when the app/process should stay alive, then call into the existing Rust `spawn_onion_thread` entry (JNI / Dioxus mobile hook). Keep Tor **off** the UI thread.

## 4. On-device check

1. Open app — Tor starts automatically (banner shows status).
2. Wait for onion online; open **Profile** → show invite QR (or copy URI).
3. On a second install/device, **Add** → **Scan QR** (allow camera) or paste URI → send request.
4. Accept on the first device; exchange text messages.
5. Optional: `torsocks curl http://<id>.onion/v1/health` from a desktop Tor client.

## 5. If Arti won’t cross-compile

- Confirm `NDK_HOME` and rustup android targets.
- `static-sqlite` is already enabled on `arti-client`.
- Watch for missing `libssl.so` in the APK ([dioxus#5565](https://github.com/DioxusLabs/dioxus/issues/5565)).

## 6. Sanity without Android

```bash
cargo run --bin headless -- demo-crypto
cargo run --bin headless
```


## 6. Launcher icon / in-app logo

- In-app logo: `assets/logo.png` (and `assets/logo-bar.png` for the top bar). Wired into Settings and the Chats app bar as a data-URI image.
- Source artwork also kept as `assets/notchat-logo.jpg` / `assets/logo.jpg`.
- **Android launcher icon** (replaces dx's default Dioxus icon): `android/tools/make_launcher_icons.py`
  generates from `assets/notchat-logo.png` into `android/overlay/res/`:
  - adaptive icon `mipmap-anydpi-v26/ic_launcher.xml` (+ `ic_launcher_round.xml`):
    background `@color/notchat_icon_background` = `#282A36` (`values/notchat_icon.xml`),
    foreground `mipmap-*/ic_launcher_foreground.webp` (108dp: 108…432 px; logo interior padded with its
    edge colour so the bubble stays inside the 66dp safe zone);
  - legacy `mipmap-{mdpi,hdpi,xhdpi,xxhdpi,xxxhdpi}/ic_launcher.webp` (48…192 px rounded square) and
    `ic_launcher_round.webp` for API < 26.
  The build script runs the generator, then `android/patch_gradle_project.py` copies `android/overlay/res/**`
  over the dx-generated `app/src/main/res` (same file names → dx's icons are overwritten) before Gradle runs.
  `android:icon="@mipmap/ic_launcher"` stays in `android/AndroidManifest.xml`.
  If the launcher still shows the old icon after an upgrade, it is launcher cache — reboot or clear launcher cache.
- Display name: set `Dioxus.toml` `[application] name = "NotChat"` (web title too). **Note:** current `dx` still often generates `app_name` from the crate name (`onion-chat` → `OnionChat`). After `dx build`, patch `target/dx/onion-chat/.../res/values/strings.xml` to `NotChat` and run `./gradlew :app:assembleDebug` (or keep a checked-in override at `android/res/values/strings.xml` and copy it over). Package id stays `dev.zarkones.onion_chat`.


## 7. Build scripts (debug / release-production)

```bash
android/build-debug-apk.sh               # debug   -> ./app-debug-arm64.apk
android/build-release-apk.sh             # release -> ./app-release-arm64.apk  (== build-debug-apk.sh --release)
ADB_SERIAL=R5CWC4EKP5F android/build-release-apk.sh   # build + adb -s … install -r
```

Release = `dx build --platform android --features mobile --target aarch64-linux-android --release`,
overlay patch, then `./gradlew :app:assembleRelease` in `target/dx/onion-chat/release/android/app`
(R8 minify on; keep rules in `android/overlay/kotlin/dev/dioxus/main/proguard-notchat.pro` keep
`dev.dioxus.main.**` because Rust calls `NotChatBridge` via JNI by name).

### Release signing key (local)

- Keystore file: `android/notchat-release.keystore` (gitignored; create locally)
- Alias: set in `android/keystore.properties` as `keyAlias` (example: `notchat`)
- Store / key passwords: **do not commit** — put them only in gitignored `android/keystore.properties`, e.g.
  `storePassword=YOUR_STORE_PASSWORD_HERE` and `keyPassword=YOUR_KEY_PASSWORD_HERE`
- Or override with env: `NOTCHAT_KEYSTORE`, `NOTCHAT_KEYSTORE_PASSWORD`, `NOTCHAT_KEY_ALIAS`, `NOTCHAT_KEY_PASSWORD`
- Signing is injected via `-Pandroid.injected.signing.*` (dx regenerates build.gradle.kts every build)
- **Back the keystore up privately.** Same key required for in-place upgrades. Never commit keystore or passwords; use a separate upload key for Play Store.


**Debug → release switch:** a phone with the *debug*-signed build (Android debug key) cannot be
upgraded to the release-signed APK (`INSTALL_FAILED_UPDATE_INCOMPATIBLE`, signature mismatch).
`adb uninstall dev.zarkones.onion_chat` first — this **wipes app data** (identity / contacts / messages)
unless exported beforehand.
