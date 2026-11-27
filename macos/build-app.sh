#!/bin/bash
# Builds SideMic.app in target/release (or target/universal with --universal).
#
#   macos/build-app.sh                     app for this Mac's CPU
#   macos/build-app.sh --universal         one app for Apple silicon and Intel Macs
#   macos/build-app.sh --universal --dmg   ...plus SideMic-<version>.dmg next to it, for GitHub releases
#
# Needs: Xcode command line tools (xcode-select --install), Rust (rustup.rs), CMake (brew install cmake).
set -euo pipefail
cd "$(dirname "$0")/.."

# rustup only puts cargo on PATH for shells opened after it was installed; pick it up anyway.
if ! command -v cargo >/dev/null && [[ -f "$HOME/.cargo/env" ]]; then
    . "$HOME/.cargo/env"
fi
command -v cargo >/dev/null || { echo "cargo not found. Install Rust from https://rustup.rs first."; exit 1; }

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

universal=0
dmg=0
for arg in "$@"; do
    case "$arg" in
        --universal) universal=1 ;;
        --dmg) dmg=1 ;;
        *) echo "unknown option: $arg"; exit 2 ;;
    esac
done

if [[ $universal == 1 ]]; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin
    cargo build --release --target aarch64-apple-darwin
    cargo build --release --target x86_64-apple-darwin
    out=target/universal
    mkdir -p "$out"
    lipo -create -output "$out/sidemic" \
        target/aarch64-apple-darwin/release/sidemic target/x86_64-apple-darwin/release/sidemic
else
    cargo build --release
    out=target/release
fi

app="$out/SideMic.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$out/sidemic" "$app/Contents/MacOS/sidemic"
sed "s/@VERSION@/$version/g" macos/Info.plist > "$app/Contents/Info.plist"

# Finder/Dock icon (best effort; the app works without one). macos/SideMic.png is the 256 px image from sidemic.ico.
iconset="$(mktemp -d)/SideMic.iconset"
mkdir -p "$iconset"
for s in 16 32 128 256; do
    sips -z $s $s macos/SideMic.png --out "$iconset/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) macos/SideMic.png --out "$iconset/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/SideMic.icns" || echo "warning: could not build the app icon"

# Ad-hoc signature. Apple silicon refuses to run unsigned code, and the microphone permission is tied to the
# signature, so the bundle (with its Info.plist) has to be signed as a whole.
codesign --force --deep --sign - "$app"

echo "Built $app"

if [[ $dmg == 1 ]]; then
    # The usual drag-to-install disk image: the app plus a shortcut to /Applications.
    staging=$(mktemp -d)
    cp -R "$app" "$staging/"
    ln -s /Applications "$staging/Applications"
    image="$out/SideMic-$version.dmg"
    hdiutil create -volname "SideMic $version" -srcfolder "$staging" -fs HFS+ -format UDZO -ov "$image" >/dev/null
    rm -rf "$staging"
    echo "Built $image"
fi

echo "Copy SideMic.app to /Applications, then open it."
