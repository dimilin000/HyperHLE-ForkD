#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../.." && pwd)
SDK=${1:-iphonesimulator}
CONFIGURATION=${2:-Release}

case "$SDK" in
    iphonesimulator)
        DESTINATION='generic/platform=iOS Simulator'
        RUST_TARGET='aarch64-apple-ios-sim'
        ;;
    iphoneos)
        DESTINATION='generic/platform=iOS'
        RUST_TARGET='aarch64-apple-ios'
        ;;
    *)
        echo "Usage: $0 [iphonesimulator|iphoneos] [Debug|Release]" >&2
        exit 2
        ;;
esac

case "$CONFIGURATION" in
    Debug|Release)
        ;;
    *)
        echo "Usage: $0 [iphonesimulator|iphoneos] [Debug|Release]" >&2
        exit 2
        ;;
esac

DEVELOPER_DIR=${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}
export DEVELOPER_DIR

# Empty, not inherited: this call always builds this repository's own core.
TOUCHHLE_CORE_REPO= sh "$ROOT/scripts/build-rust.sh" "$RUST_TARGET" "$CONFIGURATION"

# The app ships every core it is built with. TOUCHHLE_CORE_REPO points at a
# checkout of the other core (johnny901901901/touchHLE, branch ios-core-dylib);
# without it the app is built with HyperHLE alone and the core picker hides
# itself.
if [ -n "${TOUCHHLE_CORE_REPO:-}" ]; then
    TOUCHHLE_CORE_REPO="$TOUCHHLE_CORE_REPO" \
        sh "$ROOT/scripts/build-rust.sh" "$RUST_TARGET" "$CONFIGURATION"
fi

# The app's version follows the repository's release tags (vMAJOR.MINOR.PATCH,
# see dev-scripts/hyperhle-should-release.sh). CI sets HYPERHLE_VERSION to the
# version being released; otherwise the latest tag is used. The build number is
# the commit count, so every build is distinct. Without tags, the defaults in
# Config/Build.xcconfig apply.
set --
version_tag=${HYPERHLE_VERSION:-$(git -C "$REPO" describe --tags --abbrev=0 \
    --match 'v[0-9]*.[0-9]*.[0-9]*' 2>/dev/null || true)}
if [ -n "$version_tag" ]; then
    set -- "$@" "MARKETING_VERSION=${version_tag#v}"
fi
build_number=$(git -C "$REPO" rev-list --count HEAD 2>/dev/null || true)
if [ -n "$build_number" ]; then
    set -- "$@" "CURRENT_PROJECT_VERSION=$build_number"
fi
echo "App version: ${version_tag:-(from Build.xcconfig)}, build ${build_number:-(from Build.xcconfig)}"

xcodebuild \
    -project "$ROOT/TouchHLEHost.xcodeproj" \
    -scheme TouchHLEHost \
    -configuration "$CONFIGURATION" \
    -sdk "$SDK" \
    -destination "$DESTINATION" \
    -derivedDataPath "$REPO/build/host-$SDK" \
    CODE_SIGNING_ALLOWED=NO \
    "$@" \
    build
