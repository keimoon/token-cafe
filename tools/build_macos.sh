#!/bin/sh
# Build a local, unsigned app bundle for Finder / Dock launching.
set -eu
if [ "$(uname -s)" != Darwin ]; then
    echo 'This script must run on macOS.' >&2
    exit 1
fi
cd "$(dirname "$0")/.."
cargo build --locked --release
build_dir="${CARGO_TARGET_DIR:-target}/release"
app_dir="$build_dir/Token Café.app"
mkdir -p "$app_dir/Contents/MacOS"
cp "$build_dir/token-cafe" "$app_dir/Contents/MacOS/token-cafe"
cat > "$app_dir/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleExecutable</key><string>token-cafe</string>
    <key>CFBundleIdentifier</key><string>local.token-cafe.desktop</string>
    <key>CFBundleName</key><string>Token Café</string>
    <key>CFBundleDisplayName</key><string>Token Café</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>1</string>
    <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
printf 'Built %s\n' "$app_dir"
