#!/bin/zsh
# Build Kipimo.app for Apple silicon and sign it (Developer ID, hardened runtime).
#   scripts/bundle.sh             build + sign into dist/Kipimo.app
#   scripts/bundle.sh --install   also copy to /Applications and relaunch
#   scripts/bundle.sh --notarize  also notarize (needs a notarytool keychain profile named "kipimo")
set -euo pipefail
cd "${0:A:h}/.."

INSTALL=0; NOTARIZE=0
for a in "$@"; do
  case $a in
    --install) INSTALL=1 ;;
    --notarize) NOTARIZE=1 ;;
    *) echo "unknown option $a"; exit 2 ;;
  esac
done

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
BUILD=$(git rev-list --count HEAD 2>/dev/null || echo 1)
IDENTITY=${KIPIMO_SIGN_IDENTITY:-"Developer ID Application: Your Name (TEAMID)"}

cargo build --release --target aarch64-apple-darwin -p kipimo

APP=dist/Kipimo.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/aarch64-apple-darwin/release/kipimo "$APP/Contents/MacOS/Kipimo"
sed -e "s/__VERSION__/$VERSION/" -e "s/__BUILD__/$BUILD/" resources/Info.plist > "$APP/Contents/Info.plist"
[[ -f assets/AppIcon.icns ]] || swift scripts/make-icon.swift
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"

if security find-identity -v -p codesigning | grep -q "$IDENTITY"; then
  codesign --force --options runtime --timestamp=none --sign "$IDENTITY" "$APP"
else
  echo "warning: signing identity not found, using ad-hoc signature"
  codesign --force --options runtime --sign - "$APP"
fi
codesign --verify --strict "$APP"

if (( NOTARIZE )); then
  ditto -c -k --keepParent "$APP" dist/Kipimo.zip
  xcrun notarytool submit dist/Kipimo.zip --keychain-profile kipimo --wait
  xcrun stapler staple "$APP"
fi

echo "Built $APP ($(du -sh "$APP" | cut -f1), binary $(du -h "$APP/Contents/MacOS/Kipimo" | cut -f1), $(lipo -archs "$APP/Contents/MacOS/Kipimo"))"

if (( INSTALL )); then
  pkill -x Kipimo 2>/dev/null || true
  rm -rf /Applications/Kipimo.app
  ditto "$APP" /Applications/Kipimo.app
  open /Applications/Kipimo.app
  echo "Installed to /Applications/Kipimo.app"
fi
