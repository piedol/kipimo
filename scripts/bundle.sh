#!/bin/zsh
# Build Kipimo.app for Apple silicon and sign it (Developer ID, hardened runtime).
#   scripts/bundle.sh             build + sign into dist/Kipimo.app
#   scripts/bundle.sh --install   also copy to /Applications and relaunch
#   scripts/bundle.sh --notarize  also notarize and zip for release (keychain profile $NOTARY_PROFILE, default piedol-notary)
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
# Sign with $KIPIMO_SIGN_IDENTITY, or else the first Developer ID Application certificate
# in the keychain. Without either, the app gets an ad-hoc signature.
# Notarization needs a secure timestamp; everyday builds skip it to stay fast and work offline.
TS=--timestamp=none; (( NOTARIZE )) && TS=--timestamp
IDENTITY=${KIPIMO_SIGN_IDENTITY:-$(security find-identity -v -p codesigning | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)}

# Keep build-machine paths (home folder, checkout) out of the binary's panic messages.
# The encoded form separates flags with 0x1f, so paths with spaces work.
export CARGO_ENCODED_RUSTFLAGS="--remap-path-prefix=$HOME/.cargo=/cargo"$'\x1f'"--remap-path-prefix=$PWD=/kipimo"
cargo build --release --target aarch64-apple-darwin -p kipimo

APP=dist/Kipimo.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/aarch64-apple-darwin/release/kipimo "$APP/Contents/MacOS/Kipimo"
sed -e "s/__VERSION__/$VERSION/" -e "s/__BUILD__/$BUILD/" resources/Info.plist > "$APP/Contents/Info.plist"
[[ -f assets/AppIcon.icns ]] || swift scripts/make-icon.swift
cp assets/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
# Translations: locales/<lang>.json becomes <lang>.lproj/Localizable.strings. The empty
# en.lproj lists English among Kipimo's languages, so macOS can choose it.
mkdir -p "$APP/Contents/Resources/en.lproj"
printf '{}' > "$APP/Contents/Resources/en.lproj/Localizable.strings"
plutil -convert binary1 "$APP/Contents/Resources/en.lproj/Localizable.strings"
for catalog in locales/*.json; do
  lang=$(basename "$catalog" .json)
  mkdir -p "$APP/Contents/Resources/$lang.lproj"
  plutil -convert binary1 -o "$APP/Contents/Resources/$lang.lproj/Localizable.strings" "$catalog"
done

if [[ -n $IDENTITY ]] && security find-identity -v -p codesigning | grep -qF -- "\"$IDENTITY\""; then
  codesign --force --options runtime "$TS" --sign "$IDENTITY" "$APP"
else
  echo "warning: signing identity not found, using ad-hoc signature"
  codesign --force --options runtime --sign - "$APP"
fi
codesign --verify --strict "$APP"

if (( NOTARIZE )); then
  # Notarization needs a keychain profile made once with:
  #   xcrun notarytool store-credentials <profile> --apple-id <Apple ID> --team-id <team ID>
  PROFILE=${NOTARY_PROFILE:-piedol-notary}
  ditto -c -k --keepParent "$APP" dist/Kipimo-notarize.zip
  xcrun notarytool submit dist/Kipimo-notarize.zip --keychain-profile "$PROFILE" --wait
  xcrun stapler staple "$APP"
  xcrun stapler validate "$APP"
  rm -f dist/Kipimo-notarize.zip
  ditto -c -k --keepParent "$APP" dist/Kipimo-macOS-arm64.zip
  echo "Notarized: dist/Kipimo-macOS-arm64.zip"
fi

echo "Built $APP ($(du -sh "$APP" | cut -f1), binary $(du -h "$APP/Contents/MacOS/Kipimo" | cut -f1), $(lipo -archs "$APP/Contents/MacOS/Kipimo"))"

if (( INSTALL )); then
  pkill -x Kipimo 2>/dev/null || true
  rm -rf /Applications/Kipimo.app
  ditto "$APP" /Applications/Kipimo.app
  open /Applications/Kipimo.app
  echo "Installed to /Applications/Kipimo.app"
fi
