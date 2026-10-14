#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if [ "$(uname -s)" != Darwin ]; then
    echo 'This script packages the macOS application. Use cargo build on Linux.' >&2
    exit 1
fi
if ! command -v cargo >/dev/null 2>&1; then
    toolchain="${RUSTUP_HOME:-$HOME/.rustup}/toolchains/stable-$(uname -m)-apple-darwin/bin"
    # Rust calls the Apple Silicon architecture aarch64.
    if [ "$(uname -m)" = arm64 ]; then
        toolchain="${RUSTUP_HOME:-$HOME/.rustup}/toolchains/stable-aarch64-apple-darwin/bin"
    fi
    PATH="$toolchain:$PATH"
    export PATH
fi
profile=release
if [ "${1:-}" = --debug ]; then
    profile=debug
    cargo build --locked
else
    cargo build --locked --release
fi
bundle=target/Pueo.app
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp "target/$profile/pueo" "$bundle/Contents/MacOS/pueo"
icons=$(mktemp -d)
qlmanage -t -s 1024 -o "$icons" data/icons/hicolor/scalable/apps/io.github.pueo.Pueo.svg >/dev/null
mkdir "$icons/Pueo.iconset"
for size in 16 32 128 256 512; do
    sips -z $size $size "$icons/io.github.pueo.Pueo.svg.png" --out "$icons/Pueo.iconset/icon_${size}x${size}.png" >/dev/null
    sips -z $((size * 2)) $((size * 2)) "$icons/io.github.pueo.Pueo.svg.png" --out "$icons/Pueo.iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$icons/Pueo.iconset" -o "$bundle/Contents/Resources/Pueo.icns"
rm -rf "$icons"
cat > "$bundle/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleName</key><string>Pueo</string>
<key>CFBundleDisplayName</key><string>Pueo</string>
<key>CFBundleIdentifier</key><string>io.github.pueo.Pueo</string>
<key>CFBundleVersion</key><string>0.1.0</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleExecutable</key><string>pueo</string>
<key>CFBundleIconFile</key><string>Pueo</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>NSHighResolutionCapable</key><true/>
<key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
printf 'Built %s/%s\n' "$PWD" "$bundle"
