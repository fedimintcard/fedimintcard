#!/usr/bin/env bash
# Build fedimint-card-pos-core for the POS apps (docs/POS-OFFLINE.md), profile `mobile`, --locked.
#
#   pos-core-mobile.sh android    arm64-v8a and x86_64 shared libraries (cargo-ndk, API 26, 16 KB
#                                 pages), in target/pos-core-mobile/android/jniLibs/<abi>/
#   pos-core-mobile.sh ios        static libraries for aarch64-apple-ios, aarch64-apple-ios-sim and
#                                 x86_64-apple-ios, in target/pos-core-mobile/ios/<target>/
#   pos-core-mobile.sh bindings [swift|kotlin]
#                                 Swift and Kotlin bindings (UniFFI), from a host build of the
#                                 library, in target/pos-core-mobile/bindings/{swift,kotlin}/
#   pos-core-mobile.sh check      the core links no fedimint-client, fedimint-api-client or
#                                 librocksdb-sys (fails if `cargo tree` finds one)
#
# Android needs cargo-ndk (`cargo install cargo-ndk`) and an NDK in ANDROID_NDK_HOME (or
# ANDROID_NDK_LATEST_HOME, as on GitHub's runners). iOS needs Xcode. Each step prints the sizes of
# what it built and appends them to target/pos-core-mobile/sizes.txt. The library stays a `lib`
# crate: the mobile crate types are asked of `cargo rustc`. CARGO_TARGET_DIR is honoured.
set -euo pipefail

cd "$(dirname "$0")/.."
PKG=fedimint-card-pos-core
LIB=fedimint_card_pos_core
TARGET="${CARGO_TARGET_DIR:-target}"
OUT="$TARGET/pos-core-mobile"
API=26
CARGO_FLAGS=(-p "$PKG" --lib --features ffi --profile mobile --locked)

sizes() {
	find "$1" -type f \( -name '*.a' -o -name '*.so' -o -name '*.swift' -o -name '*.h' -o -name '*.kt' \) -print0 |
		xargs -0 ls -l | awk '{ printf "%10d  %s\n", $5, $9 }' | tee -a "$OUT/sizes.txt"
}

android() {
	command -v cargo-ndk >/dev/null || { echo "cargo-ndk is missing: cargo install cargo-ndk" >&2; exit 1; }
	export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-${ANDROID_NDK_LATEST_HOME:-}}"
	[ -n "$ANDROID_NDK_HOME" ] || { echo "set ANDROID_NDK_HOME to an NDK" >&2; exit 1; }
	# aws-lc-sys (through fedimint-core) builds with CMake, which finds the NDK's toolchain here.
	export ANDROID_NDK_ROOT="$ANDROID_NDK_HOME"
	local prebuilt
	prebuilt="$(find "$ANDROID_NDK_HOME/toolchains/llvm/prebuilt" -mindepth 1 -maxdepth 1 -type d | head -n 1)"
	for abi in arm64-v8a x86_64; do
		case "$abi" in
			arm64-v8a) triple=aarch64-linux-android ;;
			x86_64) triple=x86_64-linux-android ;;
		esac
		# aws-lc-sys runs bindgen with the unversioned triple, which NDK r28 and later refuse
		# ("Unversioned target triples are not supported"). bindgen reads the hyphenated
		# variable before the one cargo-ndk sets, so this one carries cargo-ndk's sysroot
		# arguments and the triple with its API level. The link aligns the library's segments to
		# 16 KB pages, which Android 15 devices may use.
		env "BINDGEN_EXTRA_CLANG_ARGS_$triple=--sysroot=$prebuilt/sysroot -I$prebuilt/sysroot/usr/include/$triple --target=$triple$API" \
			cargo ndk -t "$abi" -P "$API" rustc "${CARGO_FLAGS[@]}" --crate-type cdylib -- -C link-arg=-Wl,-z,max-page-size=16384
		mkdir -p "$OUT/android/jniLibs/$abi"
		cp "$TARGET/$triple/mobile/lib$LIB.so" "$OUT/android/jniLibs/$abi/"
	done
	sizes "$OUT/android"
}

ios() {
	for target in aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios; do
		cargo rustc "${CARGO_FLAGS[@]}" --crate-type staticlib --target "$target"
		mkdir -p "$OUT/ios/$target"
		cp "$TARGET/$target/mobile/lib$LIB.a" "$OUT/ios/$target/"
	done
	sizes "$OUT/ios"
}

bindings() {
	# The bindings come from the library's UniFFI metadata symbols, which the stripped `mobile`
	# profile drops on Linux: build the host library unstripped (the default dev profile).
	cargo rustc -p "$PKG" --lib --features ffi --locked --crate-type cdylib
	local lib
	case "$(uname -s)" in
		Darwin) lib="$TARGET/debug/lib$LIB.dylib" ;;
		*) lib="$TARGET/debug/lib$LIB.so" ;;
	esac
	for lang in ${1:-swift kotlin}; do
		rm -rf "$OUT/bindings/$lang"
		cargo run -p "$PKG" --features bindgen --bin uniffi-bindgen --locked -- \
			generate --library "$lib" --language "$lang" --out-dir "$OUT/bindings/$lang"
	done
	sizes "$OUT/bindings"
}

check() {
	local bad=0
	for dep in fedimint-client fedimint-api-client librocksdb-sys; do
		if cargo tree --locked -p "$PKG" --features ffi --target all -e normal,build -i "$dep" >/dev/null 2>&1; then
			echo "$PKG depends on $dep:" >&2
			cargo tree --locked -p "$PKG" --features ffi --target all -e normal,build -i "$dep" >&2
			bad=1
		else
			echo "no $dep in $PKG"
		fi
	done
	return "$bad"
}

mkdir -p "$OUT"
case "${1:-}" in
	android | ios | check) "$1" ;;
	bindings) bindings "${2:-}" ;;
	*)
		echo "usage: $0 android|ios|bindings [swift|kotlin]|check" >&2
		exit 2
		;;
esac
