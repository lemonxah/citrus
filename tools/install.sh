#!/usr/bin/env bash
# Keep the hub/editor siblings and Steam Audio beside the editor ($ORIGIN rpath).
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "$(uname -s)" != Linux ]]; then
    echo "Local installation currently supports Linux." >&2
    exit 1
fi
prefix=${1:-/usr}
privilege=${2-sudo}
elevate=()
if [[ -n "$privilege" ]]; then elevate=("$privilege"); fi
target_dir=$(cargo metadata --offline --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
release="$target_dir/release"
libdir="$prefix/lib/zestengine"

# The first editor build can finish before its build script sees audionimbus.
phonon="$release/libphonon.so"
if [[ ! -f "$phonon" ]]; then
    phonon=$(find "$release/build" -path '*/audionimbus-sys-*/out/lib/libphonon.so' -print -quit)
fi
for binary in zestengine zestengine-hub zestengine-relay; do test -f "$release/$binary"; done
if [[ ! -f "$phonon" ]]; then
    echo "Steam Audio libphonon.so is missing; rebuild the editor before installing." >&2
    exit 1
fi
for binary in zestengine zestengine-hub zestengine-relay; do
    "${elevate[@]}" install -Dm755 "$release/$binary" "$libdir/$binary"
done
"${elevate[@]}" install -Dm755 "$phonon" "$libdir/libphonon.so"
"${elevate[@]}" install -d "$prefix/bin"
for binary in zestengine zestengine-hub zestengine-relay; do
    "${elevate[@]}" ln -sfn "../lib/zestengine/$binary" "$prefix/bin/$binary"
done
"${elevate[@]}" install -Dm644 pkg/zestengine.desktop "$prefix/share/applications/zestengine.desktop"
"${elevate[@]}" install -Dm644 pkg/zestengine.svg "$prefix/share/icons/hicolor/scalable/apps/zestengine.svg"
if command -v gtk-update-icon-cache >/dev/null; then
    "${elevate[@]}" gtk-update-icon-cache -q -t "$prefix/share/icons/hicolor" || true
fi
echo "Installed ZestEngine editor, Project Hub and relay under $prefix."
