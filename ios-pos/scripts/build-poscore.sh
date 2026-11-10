#!/usr/bin/env bash
# Build the POS core (host/pos-core, docs/POS-OFFLINE.md) for this app, in one run:
#
#   1. host/scripts/pos-core-mobile.sh ios: the static libraries for aarch64-apple-ios,
#      aarch64-apple-ios-sim and x86_64-apple-ios;
#   2. host/scripts/pos-core-mobile.sh bindings swift: the Swift bindings, from the same tree;
#   3. Packages/FedimintCardPosCore/FedimintCardPosCoreFFI.xcframework: the device library, and the
#      two simulator libraries as one (lipo), each with the FFI header and its module map;
#   4. Packages/FedimintCardPosCore/Sources/FedimintCardPosCore/FedimintCardPosCore.swift: the
#      generated bindings.
#
# The library and the bindings must come from one build: UniFFI checks their checksums when the
# app first calls the core and stops the app (fatalError) when they differ. Both outputs are
# gitignored; Package.swift is committed. Needs Xcode and the three Rust targets
# (rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios). CARGO_TARGET_DIR is
# honoured, relative to host/ as the host script reads it.
set -euo pipefail

ios_pos="$(cd "$(dirname "$0")/.." && pwd)"
host="$(cd "$ios_pos/../host" && pwd)"
pkg="$ios_pos/Packages/FedimintCardPosCore"
target="${CARGO_TARGET_DIR:-target}"
case "$target" in /*) ;; *) target="$host/$target" ;; esac
out="$target/pos-core-mobile"
lib=libfedimint_card_pos_core.a
ffi=FedimintCardPosCoreFFI

"$host/scripts/pos-core-mobile.sh" ios
"$host/scripts/pos-core-mobile.sh" bindings swift

bindings="$out/bindings/swift"
for f in FedimintCardPosCore.swift "$ffi.h" "$ffi.modulemap"; do
	[ -f "$bindings/$f" ] || { echo "the bindings step did not write $bindings/$f" >&2; exit 1; }
done

stage="$out/ios/xcframework"
rm -rf "$stage"
mkdir -p "$stage/headers" "$stage/simulator"
cp "$bindings/$ffi.h" "$stage/headers/"
cp "$bindings/$ffi.modulemap" "$stage/headers/module.modulemap"
lipo -create "$out/ios/aarch64-apple-ios-sim/$lib" "$out/ios/x86_64-apple-ios/$lib" -output "$stage/simulator/$lib"

rm -rf "$pkg/$ffi.xcframework"
xcodebuild -create-xcframework \
	-library "$out/ios/aarch64-apple-ios/$lib" -headers "$stage/headers" \
	-library "$stage/simulator/$lib" -headers "$stage/headers" \
	-output "$pkg/$ffi.xcframework"

mkdir -p "$pkg/Sources/FedimintCardPosCore"
cp "$bindings/FedimintCardPosCore.swift" "$pkg/Sources/FedimintCardPosCore/FedimintCardPosCore.swift"
echo "FedimintCardPosCore: $pkg"
