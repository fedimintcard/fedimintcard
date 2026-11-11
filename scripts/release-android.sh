#!/usr/bin/env bash
# Build the signed Play bundles (AAB) of both Android apps: the provisioner ("Fedimint Card Setup",
# org.fedimintcard.provisioner) and the POS ("Fedimint Card POS", org.fedimintcard.pos).
#
# Signing: the Fedimint Card upload key (~/.fedimintcard-android/fedimintcard-upload.jks, alias
# fedimintcard-upload). Its password lives only in the macOS Keychain, service
# fedimintcard-android-upload; it is read into the environment of the Gradle runs and never printed.
# Play re-signs with its own app-signing key. The script's messages name the variables and never
# print the password, the alias or the keystore's path.
#
# The POS bundle carries the offline core (host/pos-core), which Gradle builds with cargo-ndk from
# the NDK in ANDROID_NDK_HOME. Unset, it defaults to the newest NDK under ~/Library/Android/sdk/ndk;
# with none there the script stops before it reads the Keychain or starts Gradle. A POS bundle
# without the core's library for arm64-v8a and x86_64 stops the script before it reaches dist/.
#
#   scripts/release-android.sh            # both
#   scripts/release-android.sh pos        # one: pos | provisioner
# Output: dist/android/<app>-<versionName>-<versionCode>.aab; the last line printed is its path.
# Runs from the repository root wherever it is called from. The whole release: docs/RELEASING.md.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
which=${1:-all}
case $which in
  all | pos | provisioner) ;;
  *) echo "usage: scripts/release-android.sh [all|pos|provisioner]" >&2; exit 2 ;;
esac

newest_ndk() { # the newest NDK under the Android SDK's default home, or nothing
  local dir
  for dir in "$HOME/Library/Android/sdk/ndk"/*/; do
    if [[ -d $dir ]]; then echo "${dir%/}"; fi
  done | sort -V | tail -n 1
}
if [[ $which == all || $which == pos ]]; then
  pos_ndk=${ANDROID_NDK_HOME:-$(newest_ndk)}
  if [[ -z $pos_ndk ]]; then
    echo "no Android NDK for the POS core: ANDROID_NDK_HOME is unset and $HOME/Library/Android/sdk/ndk has none." >&2
    echo "Install an NDK (r27 or newer) with the SDK Manager, or set ANDROID_NDK_HOME." >&2
    exit 1
  fi
  [[ -d $pos_ndk/toolchains/llvm/prebuilt ]] \
    || { echo "no NDK at $pos_ndk (it has no toolchains/llvm/prebuilt)" >&2; exit 1; }
  echo "the POS core builds with the NDK at $pos_ndk" >&2
fi

export FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE=${FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE:-$HOME/.fedimintcard-android/fedimintcard-upload.jks}
export FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS=${FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS:-fedimintcard-upload}
[ -f "$FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE" ] \
  || { echo "no upload keystore: FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE (default in scripts/release-android.sh) names no file" >&2; exit 1; }
pw=$(security find-generic-password -s fedimintcard-android-upload -a "$FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS" -w) \
  || { echo "no password in the Keychain (service fedimintcard-android-upload, account FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS)" >&2; exit 1; }
export FEDIMINTCARD_ANDROID_UPLOAD_STORE_PASSWORD=$pw FEDIMINTCARD_ANDROID_UPLOAD_KEY_PASSWORD=$pw
unset pw

out="$ROOT/dist/android"; mkdir -p "$out"
version_of() { # gradle file -> "name code"
  sed -nE 's/.*versionName[ =]+"([^"]+)".*/\1/p; s/.*versionCode[ =]+([0-9]+).*/\1/p' "$1" | paste -sd' ' -
}
pos_core_in() { # aab: stops the script unless the bundle holds the POS core for both ABIs
  local listing abi missing=()
  listing=$(unzip -l "$1")
  for abi in arm64-v8a x86_64; do
    grep -qE "[[:space:]]base/lib/$abi/libfedimint_card_pos_core\.so$" <<<"$listing" \
      || missing+=("base/lib/$abi/libfedimint_card_pos_core.so")
  done
  if ((${#missing[@]})); then
    echo "the POS bundle has no ${missing[*]}: the core did not build into it ($1)" >&2
    exit 1
  fi
}
build() { # name dir gradlefile bundle-path
  local name=$1 dir=$2 gfile=$3 aab=$4
  read -r vname vcode < <(version_of "$gfile" | awk '{print $2, $1}')
  (cd "$dir" && ./gradlew -q bundleRelease)
  if [[ $name == pos ]]; then pos_core_in "$dir/$aab"; fi
  cp "$dir/$aab" "$out/$name-$vname-$vcode.aab"
  echo "$out/$name-$vname-$vcode.aab"
}
if [[ $which == all || $which == provisioner ]]; then
  build provisioner "$ROOT/apps/provisioner/android" "$ROOT/apps/provisioner/android/app/build.gradle" app/build/outputs/bundle/release/app-release.aab
fi
if [[ $which == all || $which == pos ]]; then
  export ANDROID_NDK_HOME=$pos_ndk
  build pos "$ROOT/android-relay" "$ROOT/android-relay/app/build.gradle.kts" app/build/outputs/bundle/release/app-release.aab
fi
