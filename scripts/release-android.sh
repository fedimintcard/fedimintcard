#!/usr/bin/env bash
# Build the signed Play bundles (AAB) of both Android apps: the provisioner ("Fedimint Card Setup",
# org.fedimintcard.provisioner) and the POS ("Fedimint Card POS", org.fedimintcard.pos).
#
# Signing: the Fedimint Card upload key (~/.fedimintcard-android/fedimintcard-upload.jks, alias
# fedimintcard-upload). Its password lives only in the macOS Keychain, service
# fedimintcard-android-upload; it is read into the environment of the Gradle runs and never printed.
# Play re-signs with its own app-signing key.
#
#   scripts/release-android.sh            # both
#   scripts/release-android.sh pos        # one: pos | provisioner
# Output: dist/android/<app>-<versionName>-<versionCode>.aab
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
which=${1:-all}

export FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE=${FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE:-$HOME/.fedimintcard-android/fedimintcard-upload.jks}
export FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS=${FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS:-fedimintcard-upload}
[ -f "$FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE" ] || { echo "no upload keystore at $FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE" >&2; exit 1; }
pw=$(security find-generic-password -s fedimintcard-android-upload -a "$FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS" -w) \
  || { echo "no Keychain entry fedimintcard-android-upload / $FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS" >&2; exit 1; }
export FEDIMINTCARD_ANDROID_UPLOAD_STORE_PASSWORD=$pw FEDIMINTCARD_ANDROID_UPLOAD_KEY_PASSWORD=$pw
unset pw

out="$ROOT/dist/android"; mkdir -p "$out"
version_of() { # gradle file -> "name code"
  sed -nE 's/.*versionName[ =]+"([^"]+)".*/\1/p; s/.*versionCode[ =]+([0-9]+).*/\1/p' "$1" | paste -sd' ' -
}
build() { # name dir gradlefile bundle-path
  local name=$1 dir=$2 gfile=$3 aab=$4
  read -r vname vcode < <(version_of "$gfile" | awk '{print $2, $1}')
  (cd "$dir" && ./gradlew -q bundleRelease)
  cp "$dir/$aab" "$out/$name-$vname-$vcode.aab"
  echo "$out/$name-$vname-$vcode.aab"
}
if [[ $which == all || $which == provisioner ]]; then
  build provisioner "$ROOT/apps/provisioner/android" "$ROOT/apps/provisioner/android/app/build.gradle" app/build/outputs/bundle/release/app-release.aab
fi
if [[ $which == all || $which == pos ]]; then
  build pos "$ROOT/android-relay" "$ROOT/android-relay/app/build.gradle.kts" app/build/outputs/bundle/release/app-release.aab
fi
