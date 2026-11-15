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

cp -R "$APP_DIR" "$MOUNT_DIR/"
ln -s /Applications "$MOUNT_DIR/Applications"
if [ -f "$MACOS_DIR/Resources/AppIcon.icns" ]; then
    cp "$MACOS_DIR/Resources/AppIcon.icns" "$MOUNT_DIR/.VolumeIcon.icns" 2>/dev/null || true
    SetFile -a C "$MOUNT_DIR" 2>/dev/null || true
fi

sync
hdiutil detach "$DEVICE" -force > /dev/null
hdiutil convert "$DMG_TEMP" -format UDZO -imagekey zlib-level=9 -o "$DMG_FINAL" > /dev/null
rm -f "$DMG_TEMP"

echo "✓ Mimir.dmg generated at: $DMG_FINAL"
ls -lh "$DMG_FINAL"
