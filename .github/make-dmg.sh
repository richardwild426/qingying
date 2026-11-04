#!/bin/bash
#
# Build a drag-to-install DMG from a packaged .app.
#
#   make-dmg.sh <path to .app> <volume name> <output.dmg>
#
# The disk image holds the app next to a symlink to /Applications, so the
# user drags one onto the other — the layout Finder shows for every Mac app.
# The window's view options are written with AppleScript, which is why this
# step needs a GUI session (a GitHub macOS runner has one).

set -euo pipefail

APP_PATH="$1"
VOL_NAME="$2"
OUT_DMG="$3"

if [ ! -d "$APP_PATH" ]; then
  echo "error: no app at $APP_PATH" >&2
  exit 1
fi
APP_NAME="$(basename "$APP_PATH" .app)"

WORK="$(mktemp -d)"
STAGE="$WORK/stage"
mkdir -p "$STAGE"
MOUNT_POINT="/Volumes/$VOL_NAME"

cleanup() {
  if [ -d "$MOUNT_POINT" ]; then
    hdiutil detach "$MOUNT_POINT" -force >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

echo "→ staging $APP_NAME"
cp -R "$APP_PATH" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

# A writable image first: the layout below needs a mounted volume that can
# still be modified, which a compressed image is not.
TEMP_DMG="$WORK/temp.dmg"
SIZE_KB=$(( $(du -sk "$STAGE" | cut -f1) + 20480 ))
hdiutil create -srcfolder "$STAGE" -volname "$VOL_NAME" -fs HFS+ \
  -fsargs "-c c=64,a=16,b=16" -format UDRW -size "${SIZE_KB}k" \
  "$TEMP_DMG" >/dev/null

echo "→ mounting"
hdiutil attach "$TEMP_DMG" -readwrite -noverify -noautoopen >/dev/null
# Give the Finder a moment to notice the new volume.
sleep 2

echo "→ laying out the window"
# Position the icons and set the window's size: the arrangement a user sees
# when the image opens. Failing here is not fatal — the DMG still works.
osascript <<EOF 2>/dev/null || echo "  (window layout skipped)"
tell application "Finder"
  tell disk "$VOL_NAME"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {200, 120, 820, 520}
    set viewOptions to the icon view options of container window
    set arrangement of viewOptions to not arranged
    set icon size of viewOptions to 128
    set position of item "$APP_NAME.app" of container window to {150, 200}
    set position of item "Applications" of container window to {470, 200}
    close
  end tell
end tell
EOF
sync

echo "→ compressing"
hdiutil detach "$MOUNT_POINT" >/dev/null
hdiutil convert "$TEMP_DMG" -format UDZO -imagekey zlib-level=9 -o "$OUT_DMG" >/dev/null

echo "→ verifying"
hdiutil verify "$OUT_DMG" >/dev/null
MOUNTED="$(hdiutil attach "$OUT_DMG" -readonly -nobrowse | awk '/\/Volumes\// {print $NF}' | tail -1)"
test -d "$MOUNTED/$APP_NAME.app" || { echo "error: the app is missing from the image" >&2; exit 1; }
test -L "$MOUNTED/Applications" || { echo "error: the /Applications link is missing from the image" >&2; exit 1; }
codesign --verify --deep --strict "$MOUNTED/$APP_NAME.app"
echo "  $(lipo -archs "$MOUNTED/$APP_NAME.app/Contents/MacOS/"* | head -1)"
hdiutil detach "$MOUNTED" >/dev/null

echo "✓ $OUT_DMG ($(du -h "$OUT_DMG" | cut -f1))"
