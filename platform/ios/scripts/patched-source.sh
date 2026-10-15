#!/bin/sh
set -eu

# Usage: patched-source.sh SOURCE_DIR PATCH DESTINATION_DIR
#
# Copies SOURCE_DIR (a submodule checkout) to DESTINATION_DIR, applies PATCH to
# the copy and prints DESTINATION_DIR. The copy is only refreshed when the
# submodule revision or the patch changes, so incremental builds stay
# incremental.

SOURCE=${1:?source directory}
PATCH=${2:?patch file}
DESTINATION=${3:?destination directory}

if [ ! -f "$SOURCE/CMakeLists.txt" ]; then
    echo "$SOURCE is empty. Run: git submodule update --init" >&2
    exit 1
fi

revision=$(git -C "$SOURCE" rev-parse HEAD 2>/dev/null || echo unknown)
patch_hash=$(shasum -a 256 "$PATCH" | awk '{ print $1 }')
stamp_value="$revision $patch_hash"
STAMP="$DESTINATION/.ios-patch-stamp"

if [ ! -f "$STAMP" ] || [ "$(cat "$STAMP")" != "$stamp_value" ]; then
    echo "Preparing patched copy of $SOURCE in $DESTINATION" >&2
    rm -rf "$DESTINATION"
    mkdir -p "$DESTINATION"
    rsync -a --exclude .git "$SOURCE/" "$DESTINATION/"
    patch -p1 -s -d "$DESTINATION" <"$PATCH" >&2
    printf '%s' "$stamp_value" >"$STAMP"
fi

echo "$DESTINATION"
