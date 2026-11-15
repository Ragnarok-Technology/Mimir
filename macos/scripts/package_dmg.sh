#!/usr/bin/env bash
set -e

MACOS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_DIR="$MACOS_DIR/build"
APP_DIR="$BUILD_DIR/Mimir.app"
DMG_TEMP="$BUILD_DIR/temp.dmg"
DMG_FINAL="$BUILD_DIR/Mimir.dmg"

if [ ! -d "$APP_DIR" ]; then
    echo "Error: $APP_DIR not found. Run bundle.sh first."
    exit 1
fi

echo "==> 1. Ad-hoc codesigning Mimir.app..."
codesign --force --deep --sign - "$APP_DIR"
codesign --verify --verbose "$APP_DIR"
echo "✓ Signature valid on disk"

echo "==> 2. Creating staging disk image..."
hdiutil detach /Volumes/Mimir -force 2>/dev/null || true
rm -f "$DMG_TEMP" "$DMG_FINAL"
hdiutil create -size 45m -fs HFS+ -volname "Mimir" -ov "$DMG_TEMP" > /dev/null

ATTACH_INFO=$(hdiutil attach -readwrite -noverify -noautoopen "$DMG_TEMP")
DEVICE=$(echo "$ATTACH_INFO" | head -n 1 | awk '{print $1}')
MOUNT_DIR=$(echo "$ATTACH_INFO" | grep -o '/Volumes/.*' | head -n 1)

ROOT_DIR="$(cd "$MACOS_DIR/.." && pwd)"
BG_IMG="$ROOT_DIR/assets/brand/dmg-background.png"

if [ ! -f "$BG_IMG" ]; then
    swift "$MACOS_DIR/scripts/gen_dmg_bg.swift" "$BG_IMG"
fi

cp -R "$APP_DIR" "$MOUNT_DIR/"
ln -s /Applications "$MOUNT_DIR/Applications"
mkdir -p "$MOUNT_DIR/.background"
cp "$BG_IMG" "$MOUNT_DIR/.background/background.png"
chflags hidden "$MOUNT_DIR/.background" 2>/dev/null || true
if [ -f "$MACOS_DIR/Resources/AppIcon.icns" ]; then
    cp "$MACOS_DIR/Resources/AppIcon.icns" "$MOUNT_DIR/.VolumeIcon.icns" 2>/dev/null || true
    SetFile -a C "$MOUNT_DIR" 2>/dev/null || true
fi

echo "==> 3. Styling Finder DMG window (680x440 Refracted Obsidian)..."
osascript <<'APPLESCRIPT' || echo "Warning: Finder AppleScript styling skipped (headless environment)"
tell application "Finder"
    tell disk "Mimir"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set the bounds of container window to {180, 180, 860, 620}
        set viewOptions to the icon view options of container window
        set arrangement of viewOptions to not arranged
        set icon size of viewOptions to 112
        set text size of viewOptions to 13
        set background picture of viewOptions to file ".background:background.png"
        set position of item "Mimir.app" of container window to {190, 214}
        set position of item "Applications" of container window to {490, 214}
        close
        open
        update without registering applications
        delay 1
        close
    end tell
end tell
APPLESCRIPT

sync
hdiutil detach "$DEVICE" -force > /dev/null
hdiutil convert "$DMG_TEMP" -format UDZO -imagekey zlib-level=9 -o "$DMG_FINAL" > /dev/null
rm -f "$DMG_TEMP"

echo "✓ Mimir.dmg generated at: $DMG_FINAL"
ls -lh "$DMG_FINAL"
