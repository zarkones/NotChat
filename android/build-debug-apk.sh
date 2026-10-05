#!/usr/bin/env bash
# NotChat arm64 APK incl. foreground service, notifications and launcher icon.
#   android/build-debug-apk.sh                 # debug   -> ./app-debug-arm64.apk
#   android/build-debug-apk.sh --release       # release -> ./app-release-arm64.apk (R8 + signed)
#   android/build-release-apk.sh               # same as --release
#   ADB_SERIAL=R5CWC4EKP5F android/build-debug-apk.sh [--release]   # + adb install -r
# Release signing: android/keystore.properties (storeFile/storePassword/keyAlias/
# keyPassword); falls back to $NOTCHAT_KEYSTORE* env vars. See ANDROID.md § 7.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE=debug
for a in "$@"; do
  case "$a" in
    --release|--production) PROFILE=release ;;
    --debug) PROFILE=debug ;;
    *) echo "unknown arg: $a" >&2; exit 2 ;;
  esac
done

export JAVA_HOME="${JAVA_HOME:-$HOME/jdks/temurin-17}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export NDK_HOME="${NDK_HOME:-$(ls -d "$ANDROID_HOME"/ndk/* | sort -V | tail -1)}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$NDK_HOME}"
export PATH="$HOME/.cargo/bin:$JAVA_HOME/bin:$ANDROID_HOME/platform-tools:$PATH"

GRADLE_DIR=target/dx/onion-chat/$PROFILE/android/app
DX_FLAGS=(--platform android --features mobile --target aarch64-linux-android)
[ "$PROFILE" = release ] && DX_FLAGS+=(--release)

echo "==> launcher icons (assets/notchat-logo.png -> android/overlay/res)"
python3 android/tools/make_launcher_icons.py

echo "==> dx build ($PROFILE, aarch64)"
dx build "${DX_FLAGS[@]}"

echo "==> overlay NotChat Android sources/manifest/icons"
python3 android/patch_gradle_project.py "$GRADLE_DIR"

echo "==> check JNI exports"
NM="$NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-nm"
SO="$GRADLE_DIR/app/src/main/jniLibs/arm64-v8a/libmain.so"
if [ -x "$NM" ]; then
  # (no `grep -q`: with pipefail its early exit SIGPIPEs nm → false failure)
  if ! "$NM" -D --defined-only "$SO" | grep Java_dev_dioxus_main_NotChatBridge_nativeStartBackground >/dev/null; then
    echo "ERROR: NotChatBridge JNI symbols missing from libmain.so" >&2
    exit 1
  fi
fi

if [ "$PROFILE" = release ]; then
  PROPS=android/keystore.properties
  prop() { sed -n "s/^$1=//p" "$PROPS" 2>/dev/null | tail -1; }
  KS_FILE="${NOTCHAT_KEYSTORE:-$(prop storeFile)}"
  KS_PASS="${NOTCHAT_KEYSTORE_PASSWORD:-$(prop storePassword)}"
  KEY_ALIAS="${NOTCHAT_KEY_ALIAS:-$(prop keyAlias)}"
  KEY_PASS="${NOTCHAT_KEY_PASSWORD:-$(prop keyPassword)}"
  if [ -z "$KS_FILE" ] || [ ! -f "$KS_FILE" ]; then
    echo "ERROR: release keystore not found (android/keystore.properties / NOTCHAT_KEYSTORE)" >&2
    exit 1
  fi
  KS_FILE="$(realpath "$KS_FILE")"
  echo "==> gradle assembleRelease (signed with $KS_FILE alias $KEY_ALIAS)"
  (cd "$GRADLE_DIR" && ./gradlew --no-daemon -q :app:assembleRelease \
    -Pandroid.injected.signing.store.file="$KS_FILE" \
    -Pandroid.injected.signing.store.password="$KS_PASS" \
    -Pandroid.injected.signing.key.alias="$KEY_ALIAS" \
    -Pandroid.injected.signing.key.password="$KEY_PASS")
  APK="$GRADLE_DIR/app/build/outputs/apk/release/app-release.apk"
  OUT=app-release-arm64.apk
else
  echo "==> gradle assembleDebug"
  (cd "$GRADLE_DIR" && ./gradlew --no-daemon -q :app:assembleDebug)
  APK="$GRADLE_DIR/app/build/outputs/apk/debug/app-debug.apk"
  OUT=app-debug-arm64.apk
fi

for lib in libmain.so libssl.so libcrypto.so; do
  if ! unzip -l "$APK" | grep "lib/arm64-v8a/$lib" >/dev/null; then
    echo "ERROR: $lib missing from APK" >&2
    exit 1
  fi
done
if ! unzip -l "$APK" | grep "res/.*ic_launcher_foreground" >/dev/null && \
   ! unzip -l "$APK" | grep -E "res/[^ ]+\.webp" >/dev/null; then
  echo "WARNING: launcher icon resources not found in APK" >&2
fi
APKSIGNER="$(ls -d "$ANDROID_HOME"/build-tools/*/apksigner 2>/dev/null | sort -V | tail -1 || true)"
if [ -n "$APKSIGNER" ]; then
  "$APKSIGNER" verify --print-certs "$APK" | grep -E "Signer #1 certificate (DN|SHA-256)" || true
fi
cp "$APK" "$OUT"
ls -la "$OUT"

if [ -n "${ADB_SERIAL:-}" ]; then
  echo "==> adb -s $ADB_SERIAL install -r $OUT"
  adb -s "$ADB_SERIAL" install -r "$OUT"
fi
