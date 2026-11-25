#!/bin/bash
# Build AntiRustPaste and assemble a normal macOS .app bundle in dist/.
set -euo pipefail

cd "$(dirname "$0")"

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' apps/antirustpaste-macos/Cargo.toml | head -n1)
APP="dist/AntiRustPaste.app"
UNIVERSAL="${ANTIRUSTPASTE_UNIVERSAL:-0}"

if [ "$UNIVERSAL" = "1" ]; then
  missing=""
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    if ! rustup target list --installed 2>/dev/null | grep -qx "$target"; then
      missing="$missing $target"
    fi
  done
  if [ -n "$missing" ]; then
    echo "ANTIRUSTPASTE_UNIVERSAL=1 needs: rustup target add$missing" >&2
    exit 1
  fi
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
else
  cargo build --release
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

if [ "$UNIVERSAL" = "1" ]; then
  lipo -create \
    -output "$APP/Contents/MacOS/AntiRustPaste" \
    target/aarch64-apple-darwin/release/antirustpaste \
    target/x86_64-apple-darwin/release/antirustpaste
  echo "Architectures: $(lipo -archs "$APP/Contents/MacOS/AntiRustPaste")"
else
  cp target/release/antirustpaste "$APP/Contents/MacOS/AntiRustPaste"
fi
cp packaging/Info.plist "$APP/Contents/Info.plist"
cp assets/app-icon/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$APP/Contents/Info.plist"

# Ad-hoc signature: keeps the bundle valid and gives TCC a stable identity for
# the Accessibility permission needed to synthesise ⌘V.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "Built $APP (version $VERSION)"
