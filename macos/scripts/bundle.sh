#!/usr/bin/env bash
set -e

MACOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT_DIR="$(cd "$MACOS_DIR/.." && pwd)"
BUILD_DIR="$MACOS_DIR/build"
APP_DIR="$BUILD_DIR/Mimir.app"
CONTENTS_DIR="$APP_DIR/Contents"
MACOS_BIN_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

echo "==> 1. Building Rust mimir-cli (release)..."
(cd "$ROOT_DIR" && cargo build --release)

echo "==> 2. Building SwiftUI Mimir.app (release)..."
"$MACOS_DIR/scripts/build.sh" -c release

SWIFT_BIN="$(cd "$MACOS_DIR" && "$MACOS_DIR/scripts/build.sh" -c release --show-bin-path)/Mimir"
RUST_BIN="$ROOT_DIR/target/release/mimir"

echo "==> 3. Assembling Mimir.app bundle..."
rm -rf "$APP_DIR"
mkdir -p "$MACOS_BIN_DIR" "$RESOURCES_DIR"

cp "$SWIFT_BIN" "$MACOS_BIN_DIR/Mimir"
cp "$RUST_BIN" "$MACOS_BIN_DIR/mimir-cli"
chmod +x "$MACOS_BIN_DIR/Mimir" "$MACOS_BIN_DIR/mimir-cli"
if [ -f "$MACOS_DIR/Resources/AppIcon.icns" ]; then
    cp "$MACOS_DIR/Resources/AppIcon.icns" "$RESOURCES_DIR/AppIcon.icns"
fi

cat << 'PLIST' > "$CONTENTS_DIR/Info.plist"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>Mimir</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>xyz.fafolab.Mimir</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Mimir</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>__MIMIR_VERSION__</string>
    <key>CFBundleVersion</key>
    <string>__MIMIR_VERSION__</string>
    <key>LSMinimumSystemVersion</key>
    <string>14.0</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>Mimir captures your local microphone audio to transcribe your voice 100% locally via Whistle AI.</string>
    <key>NSScreenCaptureUsageDescription</key>
    <string>Mimir captures system speaker audio via ScreenCaptureKit so remote meeting participants are transcribed 100% locally.</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

VERSION="${MIMIR_VERSION:-0.1.0}"
VERSION="${VERSION#v}"
sed -i '' "s/__MIMIR_VERSION__/${VERSION}/g" "$CONTENTS_DIR/Info.plist"

echo "✓ Bundled $APP_DIR"
