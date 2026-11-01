#!/usr/bin/env bash
# Package a zestengine game for Android (arm64-v8a, NativeActivity, Vulkan).
#
#   tools/android/build_apk.sh [--quest] [--manifest-only] <game-crate-dir> <assets-dir> <out.apk> [package-id]
#
# <game-crate-dir> builds a cdylib exporting `android_main` (see
# examples/android-game); <assets-dir> holds project.zestengine plus the content
# folders (a Build Game `build/assets` works). Needs the NDK and build-tools:
# ANDROID_HOME (default /opt/android-sdk), ANDROID_NDK_HOME (default: newest
# under $ANDROID_HOME/ndk), and `rustup target add aarch64-linux-android`.
#
# --quest          Meta Quest 3 / 3S / Pro build: VR launcher category, head
#                  tracking feature, supported-device list, hand / eye / face
#                  tracking (optional features), and the Khronos OpenXR loader
#                  in lib/arm64-v8a.
# --manifest-only  Print the AndroidManifest.xml and exit (no build).
#
# Environment:
#   OPENXR_LOADER_SO       arm64-v8a libopenxr_loader.so to bundle (--quest).
#                          Unset: the pinned Khronos release below is fetched
#                          once into $ZESTENGINE_ANDROID_CACHE and its checksum
#                          verified. ZESTENGINE_OPENXR_FETCH=0 disables the fetch.
#   ZESTENGINE_ANDROID_CACHE   download cache (default ~/.cache/zestengine/android).
#   ZESTENGINE_QUEST2=1        also list Quest 2 as a supported device.
#   ZESTENGINE_APP_LABEL       launcher label (default: the lib name).
#   ZESTENGINE_VERSION_CODE / ZESTENGINE_VERSION_NAME   (default 1 / 0.1).
#   ZESTENGINE_KEYSTORE        release keystore; with ZESTENGINE_KEYSTORE_PASS,
#                          ZESTENGINE_KEY_ALIAS (default: the first alias) and
#                          ZESTENGINE_KEY_PASS (default: the store password).
#                          Unset: a debug key kept next to the APK (never
#                          ~/.android).
set -euo pipefail

# Pinned Khronos OpenXR loader for Android (Maven Central AAR).
OPENXR_LOADER_VERSION=1.1.63
OPENXR_LOADER_SHA256=622419d2f6741c3443a3beb4779af0764318edd01830de967f24c741ebcded73
OPENXR_LOADER_URL=https://repo1.maven.org/maven2/org/khronos/openxr/openxr_loader_for_android/$OPENXR_LOADER_VERSION/openxr_loader_for_android-$OPENXR_LOADER_VERSION.aar

QUEST=0
MANIFEST_ONLY=0
while [ $# -gt 0 ]; do
    case "$1" in
        --quest) QUEST=1; shift ;;
        --manifest-only) MANIFEST_ONLY=1; shift ;;
        --) shift; break ;;
        -*) echo "unknown option $1" >&2; exit 2 ;;
        *) break ;;
    esac
done
[ $# -ge 3 ] || { sed -n '2,4p' "$0" >&2; exit 2; }

die() { echo "error: $*" >&2; exit 1; }

CRATE=$(realpath "$1")
OUT=$(realpath -m "$3")
PACKAGE=${4:-com.zestengine.game}
LIB=$(sed -n '/^\[lib\]/,/^\[/s/^name *= *"\(.*\)"/\1/p' "$CRATE/Cargo.toml" | head -1)
[ -n "$LIB" ] || die "no [lib] name in $CRATE/Cargo.toml"
LABEL=${ZESTENGINE_APP_LABEL:-$LIB}
VERSION_CODE=${ZESTENGINE_VERSION_CODE:-1}
VERSION_NAME=${ZESTENGINE_VERSION_NAME:-0.1}

# Meta's store guidance for Horizon OS: minSdk 29 (Android 10), targetSdk 32
# (Android 12L). Phones keep the API 30 floor.
if [ $QUEST = 1 ]; then API=29; TARGET_SDK=32; else API=30; TARGET_SDK=34; fi

xml_escape() { sed -e 's/&/\&amp;/g' -e 's/</\&lt;/g' -e 's/>/\&gt;/g' -e 's/"/\&quot;/g' <<<"$1"; }

manifest() {
    local label devices
    label=$(xml_escape "$LABEL")
    cat <<XML
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="$PACKAGE"
          android:versionCode="$VERSION_CODE" android:versionName="$(xml_escape "$VERSION_NAME")" android:installLocation="auto">
    <uses-sdk android:minSdkVersion="$API" android:targetSdkVersion="$TARGET_SDK" />
XML
    if [ $QUEST = 1 ]; then
        devices="quest3|quest3s|questpro"
        [ "${ZESTENGINE_QUEST2:-0}" = 1 ] && devices="quest2|$devices"
        cat <<XML
    <uses-feature android:name="android.hardware.vulkan.version" android:version="0x401000" android:required="true" />
    <uses-feature android:name="android.hardware.vr.headtracking" android:version="1" android:required="true" />
    <uses-feature android:name="oculus.software.handtracking" android:required="false" />
    <uses-permission android:name="com.oculus.permission.HAND_TRACKING" />
    <uses-feature android:name="oculus.software.eye_tracking" android:required="false" />
    <uses-permission android:name="com.oculus.permission.EYE_TRACKING" />
    <uses-feature android:name="oculus.software.face_tracking" android:required="false" />
    <uses-permission android:name="com.oculus.permission.FACE_TRACKING" />
    <uses-permission android:name="android.permission.INTERNET" />
    <uses-permission android:name="org.khronos.openxr.permission.OPENXR" />
    <uses-permission android:name="org.khronos.openxr.permission.OPENXR_SYSTEM" />
    <queries>
        <provider android:authorities="org.khronos.openxr.runtime_broker;org.khronos.openxr.system_runtime_broker" />
        <intent>
            <action android:name="org.khronos.openxr.OpenXRRuntimeService" />
        </intent>
        <intent>
            <action android:name="org.khronos.openxr.OpenXRApiLayerService" />
        </intent>
    </queries>
    <application android:label="$label" android:hasCode="false" android:extractNativeLibs="true" android:allowBackup="false">
        <meta-data android:name="com.oculus.supportedDevices" android:value="$devices" />
        <meta-data android:name="com.oculus.handtracking.frequency" android:value="HIGH" />
        <meta-data android:name="com.oculus.handtracking.version" android:value="V2.0" />
        <activity android:name="android.app.NativeActivity" android:exported="true"
                  android:configChanges="density|keyboard|keyboardHidden|navigation|orientation|screenLayout|screenSize|uiMode"
                  android:launchMode="singleTask" android:resizeableActivity="false"
                  android:screenOrientation="landscape" android:excludeFromRecents="false"
                  android:theme="@android:style/Theme.Black.NoTitleBar.Fullscreen">
            <meta-data android:name="android.app.lib_name" android:value="$LIB" />
            <meta-data android:name="com.oculus.vr.focusaware" android:value="true" />
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="com.oculus.intent.category.VR" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
XML
    else
        cat <<XML
    <uses-feature android:name="android.hardware.vulkan.version" android:version="0x403000" android:required="true" />
    <uses-permission android:name="android.permission.INTERNET" />
    <application android:label="$label" android:hasCode="false" android:extractNativeLibs="true">
        <activity android:name="android.app.NativeActivity" android:exported="true"
                  android:configChanges="orientation|screenSize|screenLayout|keyboardHidden"
                  android:screenOrientation="landscape">
            <meta-data android:name="android.app.lib_name" android:value="$LIB" />
            <intent-filter>
                <action android:name="android.intent.action.MAIN" />
                <category android:name="android.intent.category.LAUNCHER" />
            </intent-filter>
        </activity>
    </application>
</manifest>
XML
    fi
}

if [ $MANIFEST_ONLY = 1 ]; then
    manifest
    exit 0
fi

ASSETS=$(realpath "$2")
[ -f "$ASSETS/project.zestengine" ] || die "$ASSETS has no project.zestengine (build the game's assets first)"
SDK=${ANDROID_HOME:-${ANDROID_SDK_ROOT:-/opt/android-sdk}}
[ -d "$SDK" ] || die "Android SDK not found at $SDK (set ANDROID_HOME)"
newest() { ls -d "$@" 2>/dev/null | sort -V | tail -1; }
NDK=${ANDROID_NDK_HOME:-$(newest "$SDK"/ndk/*)}
[ -n "$NDK" ] && [ -d "$NDK" ] || die "Android NDK not found (install one under $SDK/ndk or set ANDROID_NDK_HOME)"
BT=$(newest "$SDK"/build-tools/*)
[ -n "$BT" ] && [ -x "$BT/aapt2" ] || die "Android build-tools not found under $SDK/build-tools"
PLATFORM=$(newest "$SDK"/platforms/android-*)
[ -n "$PLATFORM" ] && [ -f "$PLATFORM/android.jar" ] || die "no Android platform under $SDK/platforms (install platforms;android-34 or newer)"
TC=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin
[ -x "$TC/aarch64-linux-android$API-clang" ] || die "NDK at $NDK has no aarch64-linux-android$API-clang"
rustup target list --installed 2>/dev/null | grep -qx aarch64-linux-android \
    || die "the Rust aarch64-linux-android target is missing: rustup target add aarch64-linux-android"

# The arm64 libopenxr_loader.so for a Quest build: $OPENXR_LOADER_SO, or the
# pinned Khronos AAR fetched into the cache and verified.
openxr_loader() {
    if [ -n "${OPENXR_LOADER_SO:-}" ]; then
        [ -f "$OPENXR_LOADER_SO" ] || die "OPENXR_LOADER_SO=$OPENXR_LOADER_SO does not exist"
        echo "$OPENXR_LOADER_SO"
        return
    fi
    local cache=${ZESTENGINE_ANDROID_CACHE:-$HOME/.cache/zestengine/android}/openxr_loader-$OPENXR_LOADER_VERSION
    local so=$cache/arm64-v8a/libopenxr_loader.so
    if [ ! -f "$so" ]; then
        [ "${ZESTENGINE_OPENXR_FETCH:-1}" != 0 ] \
            || die "no OpenXR loader: set OPENXR_LOADER_SO to an arm64-v8a libopenxr_loader.so (or allow the fetch)"
        command -v curl >/dev/null || die "curl is needed to fetch the OpenXR loader (or set OPENXR_LOADER_SO)"
        echo "== fetching Khronos OpenXR loader $OPENXR_LOADER_VERSION" >&2
        mkdir -p "$cache"
        local aar=$cache/loader.aar.part
        curl -sSfL -o "$aar" "$OPENXR_LOADER_URL" || die "downloading $OPENXR_LOADER_URL failed (set OPENXR_LOADER_SO)"
        echo "$OPENXR_LOADER_SHA256  $aar" | sha256sum -c --quiet - >&2 \
            || { rm -f "$aar"; die "checksum mismatch for the OpenXR loader AAR"; }
        mkdir -p "$cache/arm64-v8a"
        unzip -p "$aar" jni/arm64-v8a/libopenxr_loader.so > "$so.part" || die "the AAR has no jni/arm64-v8a/libopenxr_loader.so"
        mv "$so.part" "$so"
        rm -f "$aar"
    fi
    echo "$so"
}
LOADER=
[ $QUEST = 1 ] && LOADER=$(openxr_loader)

echo "== building lib$LIB.so (aarch64-linux-android$API)"
TARGET_DIR=${CARGO_TARGET_DIR:-$CRATE/target}
(cd "$CRATE" && env \
    CC_aarch64_linux_android="$TC/aarch64-linux-android$API-clang" \
    CXX_aarch64_linux_android="$TC/aarch64-linux-android$API-clang++" \
    AR_aarch64_linux_android="$TC/llvm-ar" \
    CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$TC/aarch64-linux-android$API-clang" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo build --release --lib --target aarch64-linux-android)

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/lib/arm64-v8a" "$STAGE/assets"
cp "$TARGET_DIR/aarch64-linux-android/release/lib$LIB.so" "$STAGE/lib/arm64-v8a/"
[ -n "$LOADER" ] && cp "$LOADER" "$STAGE/lib/arm64-v8a/libopenxr_loader.so"
cp -r "$ASSETS"/. "$STAGE/assets/"
# Extraction list read by zestengine_engine::android_assets_root.
(cd "$STAGE/assets" && find . -type f ! -name zestengine_assets.txt | sed 's|^\./||' | sort > zestengine_assets.txt)

manifest > "$STAGE/AndroidManifest.xml"

echo "== linking APK"
"$BT/aapt2" link -o "$STAGE/base.apk" --manifest "$STAGE/AndroidManifest.xml" -I "$PLATFORM/android.jar" -A "$STAGE/assets"
(cd "$STAGE" && zip -qr base.apk lib)
"$BT/zipalign" -p -f 4 "$STAGE/base.apk" "$STAGE/aligned.apk"

mkdir -p "$(dirname "$OUT")"
if [ -n "${ZESTENGINE_KEYSTORE:-}" ]; then
    [ -f "$ZESTENGINE_KEYSTORE" ] || die "ZESTENGINE_KEYSTORE=$ZESTENGINE_KEYSTORE does not exist"
    [ -n "${ZESTENGINE_KEYSTORE_PASS:-}" ] || die "ZESTENGINE_KEYSTORE is set but ZESTENGINE_KEYSTORE_PASS is not"
    echo "== signing with $ZESTENGINE_KEYSTORE"
    SIGN=(--ks "$ZESTENGINE_KEYSTORE" --ks-pass env:ZESTENGINE_KEYSTORE_PASS)
    [ -n "${ZESTENGINE_KEY_ALIAS:-}" ] && SIGN+=(--ks-key-alias "$ZESTENGINE_KEY_ALIAS")
    if [ -n "${ZESTENGINE_KEY_PASS:-}" ]; then SIGN+=(--key-pass env:ZESTENGINE_KEY_PASS); else SIGN+=(--key-pass env:ZESTENGINE_KEYSTORE_PASS); fi
else
    KEY=$(dirname "$OUT")/zestengine-debug.keystore
    if [ ! -f "$KEY" ]; then
        keytool -genkeypair -keystore "$KEY" -storepass android -keypass android -alias zestengine \
            -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=ZestEngine Debug" >/dev/null
    fi
    SIGN=(--ks "$KEY" --ks-pass pass:android --key-pass pass:android)
fi
"$BT/apksigner" sign "${SIGN[@]}" --v2-signing-enabled true --v3-signing-enabled true --out "$OUT" "$STAGE/aligned.apk"
"$BT/apksigner" verify "$OUT"
echo "== $OUT"
