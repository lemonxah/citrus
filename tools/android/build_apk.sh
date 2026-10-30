#!/usr/bin/env bash
# Package a citrus game for Android (arm64-v8a, NativeActivity, Vulkan).
#
#   tools/android/build_apk.sh <game-crate-dir> <assets-dir> <out.apk> [package-id]
#
# <game-crate-dir> builds a cdylib exporting `android_main` (see
# examples/android-game); <assets-dir> holds project.citrus plus the content
# folders (a Build Game `build/assets` works). Needs the NDK and build-tools:
# ANDROID_HOME (default /opt/android-sdk), ANDROID_NDK_HOME (default: newest
# under $ANDROID_HOME/ndk), and `rustup target add aarch64-linux-android`.
# Signs with a debug key kept next to the APK (never ~/.android).
set -euo pipefail

CRATE=$(realpath "$1")
ASSETS=$(realpath "$2")
OUT=$(realpath -m "$3")
PACKAGE=${4:-com.citrus.game}
SDK=${ANDROID_HOME:-/opt/android-sdk}
NDK=${ANDROID_NDK_HOME:-$(ls -d "$SDK"/ndk/* | sort -V | tail -1)}
BT=$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)
PLATFORM=$(ls -d "$SDK"/platforms/android-* | sort -V | tail -1)
API=30
TC=$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin

LIB=$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$CRATE/Cargo.toml" | sed -n 2p)
[ -n "$LIB" ] || { echo "no [lib] name in $CRATE/Cargo.toml" >&2; exit 1; }

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
cp -r "$ASSETS"/. "$STAGE/assets/"
# Extraction list read by citrus_engine::android_assets_root.
(cd "$STAGE/assets" && find . -type f ! -name citrus_assets.txt | sed 's|^\./||' | sort > citrus_assets.txt)

cat > "$STAGE/AndroidManifest.xml" <<XML
<?xml version="1.0" encoding="utf-8"?>
<manifest xmlns:android="http://schemas.android.com/apk/res/android" package="$PACKAGE" android:versionCode="1" android:versionName="0.1">
    <uses-sdk android:minSdkVersion="$API" android:targetSdkVersion="34" />
    <uses-feature android:name="android.hardware.vulkan.version" android:version="0x403000" android:required="true" />
    <uses-permission android:name="android.permission.INTERNET" />
    <application android:label="$LIB" android:hasCode="false" android:extractNativeLibs="true">
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

echo "== linking APK"
"$BT/aapt2" link -o "$STAGE/base.apk" --manifest "$STAGE/AndroidManifest.xml" -I "$PLATFORM/android.jar" -A "$STAGE/assets"
(cd "$STAGE" && zip -qr base.apk lib)
"$BT/zipalign" -p -f 4 "$STAGE/base.apk" "$STAGE/aligned.apk"

mkdir -p "$(dirname "$OUT")"
KEY=$(dirname "$OUT")/citrus-debug.keystore
if [ ! -f "$KEY" ]; then
    keytool -genkeypair -keystore "$KEY" -storepass android -keypass android -alias citrus \
        -keyalg RSA -keysize 2048 -validity 10000 -dname "CN=Citrus Debug" >/dev/null
fi
"$BT/apksigner" sign --ks "$KEY" --ks-pass pass:android --key-pass pass:android --out "$OUT" "$STAGE/aligned.apk"
"$BT/apksigner" verify "$OUT"
echo "== $OUT"
