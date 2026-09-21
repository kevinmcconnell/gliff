#!/usr/bin/env bash
# Build Gliff.app on a Mac: the Rust session core (crates/gliff-ffi) as a
# static library, its C header, then the Swift app, bundled and signed
# into ../dist/Gliff.app. Needs Homebrew rust and cbindgen, and the Xcode
# Command Line Tools.
#
#   macos/build.sh            # release build and bundle
#   macos/build.sh test       # also run the Swift tests
#
# Set GLIFF_SIGN_IDENTITY to sign with a real identity; the default is
# ad hoc.
set -euo pipefail
# Match the app's LSMinimumSystemVersion, or the linker warns about every
# Rust object file.
export MACOSX_DEPLOYMENT_TARGET=14.0
cd "$(dirname "$0")/.."

cargo build --release --locked -p gliff-ffi
cbindgen --quiet --config crates/gliff-ffi/cbindgen.toml --crate gliff-ffi \
    --output macos/Sources/CGliff/include/gliff.h

cd macos
swift build -c release --product Gliff
if [ "${1:-}" = test ]; then
    swift test
fi

app=../dist/Gliff.app
version=$(sed -n 's/^version = "\(.*\)"/\1/p' ../Cargo.toml | head -1)
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$(swift build -c release --product Gliff --show-bin-path)/Gliff" "$app/Contents/MacOS/Gliff"
sed "s/@VERSION@/$version/g" Info.plist > "$app/Contents/Info.plist"
codesign --force --options runtime --sign "${GLIFF_SIGN_IDENTITY:--}" "$app"
echo "built $app ($version)"
