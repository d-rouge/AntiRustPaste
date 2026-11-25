#!/bin/bash
# Build RustClip and assemble a normal macOS .app bundle in dist/.
set -euo pipefail

cd "$(dirname "$0")"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' apps/rustclip-macos/Cargo.toml | head -n1)
APP="dist/RustClip.app"

cargo build --release

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp target/release/rustclip "$APP/Contents/MacOS/rustclip"
cp packaging/Info.plist "$APP/Contents/Info.plist"
cp assets/app-icon/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP/Contents/Info.plist"

# Ad-hoc signature: keeps the bundle valid and gives TCC a stable identity for
# the Accessibility permission needed to synthesise ⌘V.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "Built $APP (version $VERSION)"
