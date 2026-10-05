#!/usr/bin/env python3
"""Overlay NotChat's Android additions onto the dx-generated Gradle project.

dx 0.7.10 regenerates target/dx/onion-chat/<profile>/android/app on every
`dx build` and its manifest template has no hook for extra <service>/<receiver>
elements or custom Kotlin, so we patch after dx and re-run Gradle:

  * copy android/overlay/kotlin/** -> app/src/main/kotlin/** (MainActivity
    override + NotChatBridge/Service/BootReceiver/JobService)
  * copy android/overlay/res/**    -> app/src/main/res/**   (status-bar icon)
  * copy android/res/values/strings.xml (display name "NotChat")
  * patch AndroidManifest.xml: permissions, singleTask activity, services,
    boot receiver. Idempotent (marker comment).
  * make sure libssl.so / libcrypto.so sit next to libmain.so in jniLibs —
    incremental `dx build` sometimes bundles only libmain.so, and libmain
    links them dynamically (dlopen fails → app/service cannot start;
    dioxus#5565). Copied from dx's prebuilt OpenSSL cache.

Usage: patch_gradle_project.py <gradle-project-dir>
"""
import re
import shutil
import sys
from pathlib import Path

MARK = "<!-- notchat-overlay -->"

PERMISSIONS = [
    "android.permission.POST_NOTIFICATIONS",
    "android.permission.FOREGROUND_SERVICE",
    "android.permission.FOREGROUND_SERVICE_REMOTE_MESSAGING",
    "android.permission.RECEIVE_BOOT_COMPLETED",
    "android.permission.REQUEST_IGNORE_BATTERY_OPTIMIZATIONS",
    "android.permission.ACCESS_NETWORK_STATE",
]

COMPONENTS = f"""
        {MARK}
        <service
            android:name="dev.dioxus.main.NotChatService"
            android:exported="false"
            android:foregroundServiceType="remoteMessaging" />
        <service
            android:name="dev.dioxus.main.NotChatJobService"
            android:exported="false"
            android:permission="android.permission.BIND_JOB_SERVICE" />
        <receiver
            android:name="dev.dioxus.main.NotChatBootReceiver"
            android:exported="true">
            <intent-filter>
                <action android:name="android.intent.action.BOOT_COMPLETED" />
                <action android:name="android.intent.action.MY_PACKAGE_REPLACED" />
            </intent-filter>
        </receiver>
"""


def copy_tree(src: Path, dst: Path) -> None:
    for f in src.rglob("*"):
        if f.is_file():
            out = dst / f.relative_to(src)
            out.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(f, out)
            print(f"  overlay {out}")


def patch_manifest(path: Path) -> None:
    xml = path.read_text()
    if MARK in xml:
        print("  manifest already patched")
        return
    perms = "".join(
        f'    <uses-permission android:name="{p}" />\n'
        for p in PERMISSIONS
        if f'android:name="{p}"' not in xml
    )
    xml = xml.replace("<application", perms + "\n    <application", 1)
    # Single Activity instance: tao keeps global Android context state; a second
    # concurrent MainActivity would re-initialise it. Notification taps reuse it.
    xml = re.sub(
        r'(android:name="dev\.dioxus\.main\.MainActivity")',
        r'\1 android:launchMode="singleTask"',
        xml,
        count=1,
    )
    xml = xml.replace("</application>", COMPONENTS + "    </application>", 1)
    path.write_text(xml)
    print(f"  patched {path}")


def ensure_openssl(jni_libs: Path) -> None:
    prebuilt_root = Path.home() / ".local/share/.dx/prebuilt"
    for abi_dir in sorted(p for p in jni_libs.iterdir() if p.is_dir()):
        for lib in ("libssl.so", "libcrypto.so"):
            dst = abi_dir / lib
            if dst.exists():
                continue
            cands = sorted(prebuilt_root.glob(f"openssl-*/ssl/libs/android.{abi_dir.name}/{lib}"))
            if not cands:
                print(f"  WARNING: {lib} missing for {abi_dir.name} and no dx prebuilt found",
                      file=sys.stderr)
                continue
            shutil.copy2(cands[-1], dst)
            print(f"  openssl {dst} <- {cands[-1]}")


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    repo = Path(__file__).resolve().parent.parent
    proj = Path(sys.argv[1]).resolve()
    main_dir = proj / "app" / "src" / "main"
    if not (main_dir / "AndroidManifest.xml").exists():
        print(f"not a dx gradle project: {proj}", file=sys.stderr)
        return 1
    copy_tree(repo / "android" / "overlay" / "kotlin", main_dir / "kotlin")
    copy_tree(repo / "android" / "overlay" / "res", main_dir / "res")
    strings = repo / "android" / "res" / "values" / "strings.xml"
    if strings.exists():
        shutil.copy2(strings, main_dir / "res" / "values" / "strings.xml")
    patch_manifest(main_dir / "AndroidManifest.xml")
    if (main_dir / "jniLibs").is_dir():
        ensure_openssl(main_dir / "jniLibs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
