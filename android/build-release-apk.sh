#!/usr/bin/env bash
# Production/release NotChat APK (R8-minified, signed) -> ./app-release-arm64.apk
#   ADB_SERIAL=R5CWC4EKP5F android/build-release-apk.sh   # + adb install -r
exec "$(dirname "$0")/build-debug-apk.sh" --release "$@"
