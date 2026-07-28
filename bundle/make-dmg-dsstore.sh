#!/bin/bash
# Regenerate `bundle/dmg-DS_Store`, the pre-authored Finder view settings for the install image.
#
# This is a one-time authoring tool, NOT part of the build: `make dmg` only copies the committed
# blob into the image. Run it by hand (like `make cert`) after changing the window geometry or the
# names of the items in the image, then commit the result.
#
#   bash bundle/make-dmg-dsstore.sh
#
# Why a committed blob: icon size and window bounds live nowhere but a `.DS_Store`, and only Finder
# writes that format. Doing it here once keeps `make dmg` free of Automation permission and of a
# Finder round-trip that breaks in CI. The blob is version-independent -- it keys off the item
# names below, not the volume name -- so it stays valid until those names change.
#
# NOTE: the records are keyed by item filename, so this is authored against the release identity's
# `TabT.app`. A dev-identity image (`TabT Dev.app`) would fall back to Finder's auto-layout;
# `make dmg` always builds the release app, so that case does not arise.
set -euo pipefail

cd "$(dirname "$0")/.."

APP=TabT.app
VOLNAME="TabT dsstore authoring"
OUT=bundle/dmg-DS_Store

# Window geometry, in Finder's screen coordinates: a 560x380 content area. Icons sit on one row,
# mirrored around the horizontal centre (280), with the drag from app to /Applications reading
# left to right.
WIN_X=200; WIN_Y=120; WIN_W=560; WIN_H=360
ICON_SIZE=128
APP_POS_X=150; APP_POS_Y=170
LINK_POS_X=410; LINK_POS_Y=170

[ -d "$APP" ] || { echo "error: $APP not found -- run 'make release' first" >&2; exit 1; }

STAGE=$(mktemp -d)
IMG=$(mktemp -u).dmg
cleanup() {
    hdiutil detach "/Volumes/$VOLNAME" -quiet 2>/dev/null || true
    rm -rf "$STAGE" "$IMG"
}
trap cleanup EXIT

# A writable image, because Finder has to be able to write the .DS_Store we are here to harvest.
ditto "$APP" "$STAGE/$APP"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "$VOLNAME" -srcfolder "$STAGE" -fs HFS+ -format UDRW -ov -quiet "$IMG"
hdiutil attach "$IMG" -quiet

echo "==> asking Finder to lay out the window"
osascript <<OSA
tell application "Finder"
    tell disk "$VOLNAME"
        open
        set current view of container window to icon view
        set toolbar visible of container window to false
        set statusbar visible of container window to false
        set the bounds of container window to {$WIN_X, $WIN_Y, $((WIN_X + WIN_W)), $((WIN_Y + WIN_H))}
        set opts to the icon view options of container window
        set arrangement of opts to not arranged
        set icon size of opts to $ICON_SIZE
        set text size of opts to 12
        set label position of opts to bottom
        set position of item "$APP" of container window to {$APP_POS_X, $APP_POS_Y}
        set position of item "Applications" of container window to {$LINK_POS_X, $LINK_POS_Y}
        update without registering applications
        delay 1
        close
    end tell
end tell
OSA

# Finder flushes the .DS_Store lazily; the close above plus a sync is what makes it land on disk.
sync
sleep 2

[ -s "/Volumes/$VOLNAME/.DS_Store" ] || {
    echo "error: Finder wrote no .DS_Store -- check Automation permission for this terminal" >&2
    exit 1
}
cp "/Volumes/$VOLNAME/.DS_Store" "$OUT"

echo "==> wrote $OUT ($(wc -c < "$OUT" | tr -d ' ') bytes)"
