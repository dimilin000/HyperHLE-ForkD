#!/bin/sh
set -eu

# Builds SDL2 as a *shared* library for iOS.
#
# The host app and each emulator core need to be the same SDL: SDL owns the
# process entry point (main.m's main is really SDL_main) and the
# UIApplicationDelegate, so a core loaded with dlopen cannot bring its own
# copy. One dylib is embedded in the app bundle and everything links to it.

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
REPO=$(CDPATH= cd -- "$ROOT/../.." && pwd)
SDK=${1:-iphoneos}

case "$SDK" in
    iphoneos|iphonesimulator)
        ;;
    *)
        echo "Usage: $0 [iphoneos|iphonesimulator]" >&2
        exit 2
        ;;
esac

for command in cmake ninja xcrun; do
    if ! command -v "$command" >/dev/null 2>&1; then
        echo "Missing required command: $command" >&2
        exit 1
    fi
done

# vendor/SDL is the same SDL revision rust-sdl2 uses. The iOS host needs a few
# changes to SDL's UIKit backend (a UIWindowScene delegate, no built-in launch
# screen, and so on). They are applied to a copy so that the submodule the
# other platforms build from stays untouched.
SDL_SRC=$(sh "$ROOT/scripts/patched-source.sh" \
    "$REPO/vendor/SDL" "$ROOT/patches/sdl-ios-host.patch" "$REPO/build/sdl-ios-src")

BUILD_DIR="$REPO/build/sdl-shared-$SDK"
PREFIX="$BUILD_DIR/install"
SDKROOT=$(xcrun --sdk "$SDK" --show-sdk-path)
DEVELOPER_DIR=${DEVELOPER_DIR:-/Applications/Xcode.app/Contents/Developer}
TOUCHHLE_BOOST_ROOT=${TOUCHHLE_BOOST_ROOT:-"$REPO/vendor/boost"}
export SDKROOT DEVELOPER_DIR TOUCHHLE_BOOST_ROOT

# SDL records __FILE__ in its error and assertion strings, which would otherwise
# put the build machine's home directory in a shipped binary. The Rust build
# remaps the same prefix with --remap-path-prefix.
SDL_CFLAGS="${CFLAGS:-} -ffile-prefix-map=$HOME=/build -fdebug-prefix-map=$HOME=/build"

cmake \
    -S "$SDL_SRC" \
    -B "$BUILD_DIR" \
    -G Ninja \
    -DCMAKE_C_FLAGS="$SDL_CFLAGS" \
    -DCMAKE_OBJC_FLAGS="$SDL_CFLAGS" \
    -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
    -DCMAKE_TOOLCHAIN_FILE="$ROOT/cmake/TouchHLEiOS.cmake" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_INSTALL_PREFIX="$PREFIX" \
    -DCMAKE_INSTALL_NAME_DIR=@rpath \
    -DCMAKE_MACOSX_RPATH=ON \
    -DSDL_SHARED=ON \
    -DSDL_STATIC=OFF \
    -DSDL_TEST=OFF \
    -DSDL2_DISABLE_INSTALL=OFF

cmake --build "$BUILD_DIR" --target install

echo
echo "SDL2 installed into $PREFIX"
ls "$PREFIX/lib"
