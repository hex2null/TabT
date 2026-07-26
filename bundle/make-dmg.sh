#!/bin/bash
# Build a distributable disk image from an already-built TabT.app.
#
# Produces `dist/TabT-<version>.dmg`: a compressed (UDZO) read-only image containing the app
# next to a symlink to /Applications, i.e. the standard "drag the icon onto the folder" macOS
# install flow. No dependencies beyond the system tools (hdiutil/ditto/PlistBuddy) -- in
# particular there is no Finder/AppleScript step to set a background image or icon positions,
# which would need Automation permission and tends to break in CI.
#
# Gatekeeper: by default the app carries the local self-signed "TabT Dev" signature (or an
# ad-hoc one), which is fine on this machine but is refused on any other Mac. To ship it
# elsewhere, re-sign and notarize with a real Apple Developer ID by setting:
#
#   SIGN_ID="Developer ID Application: Your Name (TEAMID)"   # re-signs app + signs the dmg
#   NOTARY_PROFILE=tabt-notary                               # `xcrun notarytool store-credentials` keychain profile
#
# Both are optional and independent; with neither set the script still produces a working dmg.
set -euo pipefail

cd "$(dirname "$0")/.."

APP=${APP:-TabT.app}   # release identity; `make dmg` passes it explicitly
OUTDIR=${OUTDIR:-dist}
SIGN_ID=${SIGN_ID:-}
NOTARY_PROFILE=${NOTARY_PROFILE:-}

[ -d "$APP" ] || { echo "error: $APP not found -- run 'make release' first" >&2; exit 1; }

VERSION=$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$APP/Contents/Info.plist")
NAME=$(basename "$APP" .app)
VOLNAME="$NAME $VERSION"
DMG="$OUTDIR/$NAME-$VERSION.dmg"

# Hardened runtime is required for notarization, so re-signing has to happen before the app is
# copied into the image (signing it afterwards would only sign the read-only dmg wrapper).
if [ -n "$SIGN_ID" ]; then
    echo "==> re-signing $APP with '$SIGN_ID' (hardened runtime)"
    codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$APP"
fi

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

# ditto, not cp -R: it is the Apple-sanctioned way to copy a bundle and keeps the code
# signature and extended attributes intact (a cp -R'd app can fail to launch).
ditto "$APP" "$STAGE/$(basename "$APP")"
ln -s /Applications "$STAGE/Applications"

mkdir -p "$OUTDIR"
rm -f "$DMG"
hdiutil create -volname "$VOLNAME" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov -quiet "$DMG"

if [ -n "$SIGN_ID" ]; then
    codesign --force --sign "$SIGN_ID" "$DMG"
fi

if [ -n "$NOTARY_PROFILE" ]; then
    echo "==> submitting $DMG to Apple for notarization (this takes a few minutes)"
    xcrun notarytool submit "$DMG" --keychain-profile "$NOTARY_PROFILE" --wait
    # Stapling the ticket onto the dmg lets Gatekeeper verify it without a network round-trip.
    xcrun stapler staple "$DMG"
elif [ -z "$SIGN_ID" ]; then
    echo "==> note: signed with the local development identity only -- this dmg will be"
    echo "    rejected by Gatekeeper on other Macs. See the header of $0 to ship it."
fi

echo "==> built $DMG ($(du -h "$DMG" | cut -f1))"
