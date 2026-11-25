#!/bin/bash
# Build the styled disk image: app icon on the left, an arrow, and an
# Applications shortcut on the right, with our own background picture.
set -euo pipefail

cd "$(dirname "$0")/.."

APP="${1:-dist/AntiRustPaste.app}"
VERSION="${2:-$(sed -n 's/^version = "\(.*\)"/\1/p' apps/antirustpaste-macos/Cargo.toml | head -n1)}"
OUT="${3:-dist/release}"
NAME="AntiRustPaste-$VERSION-universal"
VOLUME="AntiRustPaste $VERSION"
VOLPATH="/Volumes/$VOLUME"

if [ ! -d "$APP" ]; then
  echo "make-dmg: missing app bundle: $APP" >&2
  exit 1
fi
if [ -e "$VOLPATH" ]; then
  echo "make-dmg: $VOLPATH is already mounted; eject it first" >&2
  exit 1
fi
mkdir -p "$OUT"

STAGE="$(mktemp -d)"
TMP_DMG="$OUT/$NAME-layout.dmg"

cleanup() {
  hdiutil detach "$VOLPATH" >/dev/null 2>&1 || true
  rm -rf "$STAGE"
}
trap cleanup EXIT

cp -R "$APP" "$STAGE/AntiRustPaste.app"
ln -s /Applications "$STAGE/Applications"

rm -f "$TMP_DMG" "$OUT/$NAME.dmg"
hdiutil create -quiet -volname "$VOLUME" -srcfolder "$STAGE" -ov -format UDRW "$TMP_DMG"
hdiutil attach -quiet -readwrite -noverify -noautoopen "$TMP_DMG"
sleep 2

# The background is copied while the image is mounted so Finder's alias points
# at the live file on the volume.
mkdir -p "$VOLPATH/.background"
cp assets/dmg/background.png assets/dmg/background@2x.png "$VOLPATH/.background/"

# Finder writes the icon positions, window size and background into .DS_Store.
layout="
tell application \"Finder\"
  tell disk \"$VOLUME\"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {120, 120, 780, 540}
    set viewOptions to the icon view options of container window
    set arrangement of viewOptions to not arranged
    set icon size of viewOptions to 128
    set background picture of viewOptions to file \".background:background.png\"
    set position of item \"AntiRustPaste.app\" of container window to {160, 210}
    set position of item \"Applications\" of container window to {500, 210}
    update without registering applications
    delay 2
    close
    open
    delay 1
    close
  end tell
end tell
"
for attempt in 1 2 3; do
  if osascript -e "$layout"; then
    break
  fi
  if [ "$attempt" = "3" ]; then
    echo "make-dmg: Finder did not accept the layout" >&2
    exit 1
  fi
  sleep 2
done

# Finder metadata that should not travel inside the image.
if [ ! -f "$VOLPATH/.DS_Store" ]; then
  echo "make-dmg: Finder did not write .DS_Store" >&2
  exit 1
fi
rm -rf "$VOLPATH/.fseventsd" "$VOLPATH/.Trashes"
chmod -Rf go-w "$VOLPATH" 2>/dev/null || true
# Open the volume window automatically when the image is mounted.
bless --folder "$VOLPATH" >/dev/null 2>&1 || true
sync
sleep 2
hdiutil detach -quiet "$VOLPATH"
hdiutil convert -quiet "$TMP_DMG" -format UDZO -o "$OUT/$NAME.dmg"
rm -f "$TMP_DMG"
echo "Built $OUT/$NAME.dmg"
