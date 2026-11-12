#!/usr/bin/env bash
# Tests for the release scripts (docs/RELEASING.md): scripts/release-android.sh and
# ios-pos/scripts/release-ios.sh. Each case copies a script into a scratch tree and runs it with an
# environment of its own (env -i), a scratch HOME and stand-ins for Gradle, xcodebuild, the core
# build and the Keychain that record their calls. Nothing is built, signed or uploaded, and the real
# Keychain is never read. The fixtures take this checkout's project files with test versions. PATH
# holds the stand-ins and the system directories only, so on macOS the scripts run under /bin/bash
# 3.2 and the BSD tools, as they do on the release Mac.
#
#   scripts/test-release-scripts.sh
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
scratch=$(cd "$(mktemp -d)" && pwd -P)
trap 'rm -rf "${scratch:?}"' EXIT
password=test-password-1f3a  # what the stand-in Keychain returns
key_alias=test-upload-alias  # the upload key alias every Android case sets
failed=0 cases=0

expect() { # description, then a command that succeeds when the expectation holds
  local what=$1; shift
  if "$@"; then echo "ok    $what"; else echo "FAIL  $what" >&2; failed=$((failed + 1)); fi
}
has() { grep -qF -- "$2" "$1"; }
lacks() { ! grep -qF -- "$2" "$1"; }
empty() { [[ ! -s $1 ]]; }
is() { [[ $1 == "$2" ]]; }
last_line() { tail -n 1 "$1"; }
fixture() { # source destination sed-script pattern...: a project file with test versions in it
  local src=$1 dst=$2 script=$3 pattern
  shift 3
  mkdir -p "$(dirname "$dst")"
  sed -E "$script" "$src" >"$dst"
  for pattern in "$@"; do
    grep -qE "$pattern" "$dst" || { echo "fixture: $src no longer matches $pattern" >&2; exit 2; }
  done
}
stand_in() { # path body: an executable script
  mkdir -p "$(dirname "$1")"
  printf '#!/usr/bin/env bash\nset -euo pipefail\n%s\n' "$2" >"$1"
  chmod +x "$1"
}
# shellcheck disable=SC2016 # the stand-ins' bodies expand their variables when they run
new_case() { # name: a scratch tree in $c with both scripts, their project files and stand-ins
  cases=$((cases + 1))
  c=$scratch/$cases
  echo "# $1"
  mkdir -p "$c/repo/scripts" "$c/repo/ios-pos/scripts" "$c/home" "$c/keys"
  : >"$c/log"
  cp "$repo/scripts/release-android.sh" "$c/repo/scripts/"
  cp "$repo/ios-pos/scripts/release-ios.sh" "$c/repo/ios-pos/scripts/"
  cp "$repo/ios-pos/ExportOptions.plist" "$c/repo/ios-pos/"
  fixture "$repo/android-relay/app/build.gradle.kts" "$c/repo/android-relay/app/build.gradle.kts" \
    's/^( +versionCode = )[0-9]+$/\177/; s/^( +versionName = )"[^"]+"$/\1"9.8.7"/' \
    '^ +versionCode = 77$' '^ +versionName = "9\.8\.7"$'
  mkdir -p "$c/repo/apps/provisioner/android/app"
  printf '    versionCode 5\n    versionName "4.3.2"\n' >"$c/repo/apps/provisioner/android/app/build.gradle"
  fixture "$repo/ios-pos/project.yml" "$c/repo/ios-pos/project.yml" \
    's/^( +MARKETING_VERSION: )"[^"]+"$/\1"9.8.7"/; s/^( +CURRENT_PROJECT_VERSION: )"[^"]+"$/\1"65"/' \
    '^ +MARKETING_VERSION: "9\.8\.7"$' '^ +CURRENT_PROJECT_VERSION: "65"$'
  fixture "$repo/ios-pos/FedimintCardPOS.xcodeproj/project.pbxproj" \
    "$c/repo/ios-pos/FedimintCardPOS.xcodeproj/project.pbxproj" \
    's/^([[:space:]]+MARKETING_VERSION = ).*;$/\19.8.7;/; s/^([[:space:]]+CURRENT_PROJECT_VERSION = ).*;$/\165;/' \
    '^[[:space:]]+MARKETING_VERSION = 9\.8\.7;$' '^[[:space:]]+CURRENT_PROJECT_VERSION = 65;$'
  # Gradle: records the project, the NDK and whether the Keychain's password reached it (never the
  # password itself), then writes a bundle holding the core for the ABIs in FAKE_ABIS.
  local gradlew='echo "gradlew $PWD $* ndk=${ANDROID_NDK_HOME:-unset}" >>"$FAKE_LOG"
if [[ ${FEDIMINTCARD_ANDROID_UPLOAD_STORE_PASSWORD:-} == "$FAKE_PASSWORD" && ${FEDIMINTCARD_ANDROID_UPLOAD_KEY_PASSWORD:-} == "$FAKE_PASSWORD" ]]; then
  echo "gradlew got the Keychain password" >>"$FAKE_LOG"
fi
rm -rf bundle && mkdir -p bundle/base/manifest app/build/outputs/bundle/release
echo manifest >bundle/base/manifest/AndroidManifest.xml
for abi in ${FAKE_ABIS:-}; do mkdir -p "bundle/base/lib/$abi" && echo core >"bundle/base/lib/$abi/libfedimint_card_pos_core.so"; done
rm -f app/build/outputs/bundle/release/app-release.aab
(cd bundle && zip -qr ../app/build/outputs/bundle/release/app-release.aab .)'
  stand_in "$c/repo/android-relay/gradlew" "$gradlew"
  stand_in "$c/repo/apps/provisioner/android/gradlew" "$gradlew"
  stand_in "$c/bin/security" 'echo "security $*" >>"$FAKE_LOG"
if [[ ${FAKE_KEYCHAIN:-} == missing ]]; then echo "security: The specified item could not be found in the keychain." >&2; exit 44; fi
echo "$FAKE_PASSWORD"'
  stand_in "$c/bin/xcodebuild" 'echo "xcodebuild $* (in $PWD)" >>"$FAKE_LOG"'
  stand_in "$c/repo/ios-pos/scripts/build-poscore.sh" 'echo "build-poscore.sh (in $PWD)" >>"$FAKE_LOG"
exit "${FAKE_POSCORE_STATUS:-0}"'
  echo "not a key" >"$c/keys/AuthKey_TEST.p8"
  mkdir -p "$c/home/.fedimintcard-android"
  echo "not a keystore" >"$c/home/.fedimintcard-android/fedimintcard-upload.jks"
}
ndk() { # directory: a stand-in NDK with the toolchain directory the release script looks for
  mkdir -p "$1/toolchains/llvm/prebuilt/host"
}
run() { # script [NAME=value ...] [-- args]: runs the copy from $c with a clean environment
  local script=$1 envs=()
  shift
  while (($#)) && [[ $1 != -- ]]; do envs+=("$1"); shift; done
  if (($#)); then shift; fi
  if (cd "$c" && env -i PATH="$c/bin:/usr/bin:/bin:/usr/sbin:/sbin" HOME="$c/home" \
    FAKE_LOG="$c/log" FAKE_PASSWORD="$password" ${envs[@]+"${envs[@]}"} \
    "$c/repo/$script" "$@" >"$c/out" 2>"$c/err"); then status=0; else status=$?; fi
}
android() { run scripts/release-android.sh FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS=$key_alias "$@"; }
ios() { run ios-pos/scripts/release-ios.sh "$@"; }
asc=(ASC_KEY_PATH=keys/AuthKey_TEST.p8 ASC_KEY_ID=TESTKEYID ASC_ISSUER_ID=test-issuer-id)
prints_no_secret() { # the run's output names no password, key alias or keystore path
  cat "$c/out" "$c/err" >"$c/all"
  expect "prints no password" lacks "$c/all" "$password"
  expect "prints no key alias" lacks "$c/all" "$key_alias"
  expect "prints no keystore path" lacks "$c/all" ".fedimintcard-android"
}

# --- scripts/release-android.sh ---------------------------------------------------------------

new_case "android: no ANDROID_NDK_HOME and no NDK under HOME stops before the Keychain and Gradle"
android -- pos
expect "fails" is "$status" 1
expect "names ANDROID_NDK_HOME" has "$c/err" "ANDROID_NDK_HOME is unset"
expect "names the NDK directory it searched" has "$c/err" "$c/home/Library/Android/sdk/ndk has none"
expect "reads no Keychain and runs no Gradle" empty "$c/log"
expect "writes nothing to dist" is "$(ls "$c/repo/dist" 2>/dev/null)" ""
prints_no_secret

new_case "android: 'all' needs the NDK too, since it builds the POS"
android -- all
expect "fails" is "$status" 1
expect "names ANDROID_NDK_HOME" has "$c/err" "ANDROID_NDK_HOME is unset"
expect "reads no Keychain and runs no Gradle" empty "$c/log"

new_case "android: ANDROID_NDK_HOME naming no NDK stops before the Keychain and Gradle"
android ANDROID_NDK_HOME="$c/no-ndk-here" -- pos
expect "fails" is "$status" 1
expect "names the directory" has "$c/err" "no NDK at $c/no-ndk-here"
expect "reads no Keychain and runs no Gradle" empty "$c/log"

new_case "android: an incomplete newest NDK stops the run"
ndk "$c/home/Library/Android/sdk/ndk/27.1.12297006"
mkdir -p "$c/home/Library/Android/sdk/ndk/28.0.13004108"
android -- pos
expect "fails" is "$status" 1
expect "names the newest directory" has "$c/err" "no NDK at $c/home/Library/Android/sdk/ndk/28.0.13004108"
expect "reads no Keychain and runs no Gradle" empty "$c/log"

new_case "android: pos defaults to the newest NDK, signs with the Keychain's password, prints the bundle"
for v in 26.3.11579264 28.2.13676358 28.10.0; do ndk "$c/home/Library/Android/sdk/ndk/$v"; done
android FAKE_ABIS="arm64-v8a x86_64" -- pos
expect "succeeds" is "$status" 0
expect "asks the Keychain for the upload key's password" \
  has "$c/log" "security find-generic-password -s fedimintcard-android-upload -a $key_alias -w"
expect "runs Gradle in android-relay with the newest NDK (version order)" \
  has "$c/log" "gradlew $c/repo/android-relay -q bundleRelease ndk=$c/home/Library/Android/sdk/ndk/28.10.0"
expect "hands Gradle the Keychain's password" has "$c/log" "gradlew got the Keychain password"
expect "last line is the bundle's path" is "$(last_line "$c/out")" "$c/repo/dist/android/pos-9.8.7-77.aab"
expect "the bundle is there" test -f "$c/repo/dist/android/pos-9.8.7-77.aab"
expect "says which NDK" has "$c/err" "the POS core builds with the NDK at $c/home/Library/Android/sdk/ndk/28.10.0"
prints_no_secret

new_case "android: an ANDROID_NDK_HOME that is set wins over the newest NDK"
ndk "$c/home/Library/Android/sdk/ndk/28.10.0"
ndk "$c/ndk-chosen"
android ANDROID_NDK_HOME="$c/ndk-chosen" FAKE_ABIS="arm64-v8a x86_64" -- pos
expect "succeeds" is "$status" 0
expect "Gradle gets the NDK that was set" has "$c/log" "ndk=$c/ndk-chosen"

for abi in arm64-v8a x86_64; do
  other=x86_64; [[ $abi == x86_64 ]] && other=arm64-v8a
  new_case "android: a POS bundle without the core for $abi stops before dist"
  ndk "$c/home/Library/Android/sdk/ndk/27.1.12297006"
  android FAKE_ABIS="$other" -- pos
  expect "fails" is "$status" 1
  expect "names the missing library" has "$c/err" "base/lib/$abi/libfedimint_card_pos_core.so"
  expect "does not name the library it has" lacks "$c/err" "base/lib/$other/"
  expect "copies nothing to dist" test ! -e "$c/repo/dist/android/pos-9.8.7-77.aab"
  prints_no_secret
done

new_case "android: 'all' builds the provisioner without the NDK, then the POS with it"
ndk "$c/home/Library/Android/sdk/ndk/27.1.12297006"
android FAKE_ABIS="arm64-v8a x86_64" -- all
expect "succeeds" is "$status" 0
expect "the provisioner's Gradle gets no NDK from the script" \
  has "$c/log" "gradlew $c/repo/apps/provisioner/android -q bundleRelease ndk=unset"
expect "the POS's Gradle gets the NDK" \
  has "$c/log" "gradlew $c/repo/android-relay -q bundleRelease ndk=$c/home/Library/Android/sdk/ndk/27.1.12297006"
expect "prints the provisioner's bundle" has "$c/out" "$c/repo/dist/android/provisioner-4.3.2-5.aab"
expect "last line is the POS bundle" is "$(last_line "$c/out")" "$c/repo/dist/android/pos-9.8.7-77.aab"

new_case "android: 'provisioner' needs no NDK and no core in its bundle"
android -- provisioner
expect "succeeds" is "$status" 0
expect "last line is the provisioner's bundle" \
  is "$(last_line "$c/out")" "$c/repo/dist/android/provisioner-4.3.2-5.aab"

new_case "android: a missing keystore stops before the Keychain"
ndk "$c/home/Library/Android/sdk/ndk/27.1.12297006"
rm "$c/home/.fedimintcard-android/fedimintcard-upload.jks"
android -- pos
expect "fails" is "$status" 1
expect "names the variable" has "$c/err" "FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE"
expect "reads no Keychain and runs no Gradle" empty "$c/log"
prints_no_secret

new_case "android: no Keychain entry stops before Gradle"
ndk "$c/home/Library/Android/sdk/ndk/27.1.12297006"
android FAKE_KEYCHAIN=missing -- pos
expect "fails" is "$status" 1
expect "says the Keychain has no password" has "$c/err" "no password in the Keychain"
expect "runs no Gradle" lacks "$c/log" "gradlew"
prints_no_secret

new_case "android: an unknown app is refused"
android -- pso
expect "fails with usage" is "$status" 2
expect "prints usage" has "$c/err" "usage: scripts/release-android.sh [all|pos|provisioner]"
expect "reads no Keychain and runs no Gradle" empty "$c/log"

# --- ios-pos/scripts/release-ios.sh -----------------------------------------------------------

new_case "ios: no App Store Connect key stops before the core build and xcodebuild"
ios
expect "fails" is "$status" 1
expect "names all three variables" has "$c/err" "set ASC_KEY_PATH ASC_KEY_ID ASC_ISSUER_ID"
expect "builds nothing" empty "$c/log"

for i in 0 1 2; do
  name=${asc[$i]%%=*}
  others=()
  for j in 0 1 2; do if ((j != i)); then others+=("${asc[$j]}"); fi; done
  new_case "ios: without $name it stops before building"
  ios "${others[@]}"
  expect "fails" is "$status" 1
  expect "names $name alone" is "$(cat "$c/err")" "set $name (an App Store Connect API key) to archive and upload"
  expect "builds nothing" empty "$c/log"
done

new_case "ios: a key path naming no file stops before building, without printing the values"
ios ASC_KEY_PATH="$c/keys/AuthKey_MISSING.p8" ASC_KEY_ID=TESTKEYID ASC_ISSUER_ID=test-issuer-id
expect "fails" is "$status" 1
expect "names ASC_KEY_PATH" has "$c/err" "ASC_KEY_PATH names no readable key file"
expect "prints no key path" lacks "$c/err" "AuthKey_MISSING"
expect "prints no key ID" lacks "$c/err" "TESTKEYID"
expect "prints no issuer ID" lacks "$c/err" "test-issuer-id"
expect "builds nothing" empty "$c/log"

new_case "ios: builds the core, archives and uploads with the version from project.yml"
ios "${asc[@]}"
expect "succeeds" is "$status" 0
expect "builds the core first, from the repository root" \
  is "$(sed -n 1p "$c/log")" "build-poscore.sh (in $c/repo)"
expect "archives with the version and build from project.yml, signing with the API key" is "$(sed -n 2p "$c/log")" \
  "xcodebuild -project ios-pos/FedimintCardPOS.xcodeproj -scheme FedimintCardPOS -configuration Release -destination generic/platform=iOS CODE_SIGN_STYLE=Automatic DEVELOPMENT_TEAM=H7UAM79QQP -allowProvisioningUpdates -authenticationKeyPath $c/keys/AuthKey_TEST.p8 -authenticationKeyID TESTKEYID -authenticationKeyIssuerID test-issuer-id archive -archivePath dist/ios/FedimintCardPOS-9.8.7-65.xcarchive (in $c/repo)"
expect "exports and uploads with the API key (its relative path made absolute)" is "$(sed -n 3p "$c/log")" \
  "xcodebuild -exportArchive -archivePath dist/ios/FedimintCardPOS-9.8.7-65.xcarchive -exportOptionsPlist ios-pos/ExportOptions.plist -exportPath dist/ios/export-9.8.7-65 -allowProvisioningUpdates -authenticationKeyPath $c/keys/AuthKey_TEST.p8 -authenticationKeyID TESTKEYID -authenticationKeyIssuerID test-issuer-id (in $c/repo)"
expect "runs nothing else" is "$(wc -l <"$c/log" | tr -d ' ')" 3
expect "last line is the archive's path" \
  is "$(last_line "$c/out")" "$c/repo/dist/ios/FedimintCardPOS-9.8.7-65.xcarchive"

# The core build has no switch to skip it: the core an earlier build left in ios-pos/Packages can
# be from another commit, and git does not show it.
new_case "ios: the core builds on every run, with one already in place and SKIP_POSCORE=1 set"
pkg=$c/repo/ios-pos/Packages/FedimintCardPosCore
mkdir -p "$pkg/FedimintCardPosCoreFFI.xcframework" "$pkg/Sources/FedimintCardPosCore"
echo "// an earlier build's bindings" >"$pkg/Sources/FedimintCardPosCore/FedimintCardPosCore.swift"
ios "${asc[@]}" SKIP_POSCORE=1
expect "succeeds" is "$status" 0
expect "builds the core before the archive" is "$(sed -n 1p "$c/log")" "build-poscore.sh (in $c/repo)"
expect "then archives and uploads" is "$(grep -c '^xcodebuild ' "$c/log")" 2

new_case "ios: a failed core build stops before xcodebuild"
ios "${asc[@]}" FAKE_POSCORE_STATUS=3
expect "fails" is "$status" 3
expect "runs no xcodebuild" lacks "$c/log" "xcodebuild"

for mismatch in "CURRENT_PROJECT_VERSION 65 64" "MARKETING_VERSION 9.8.7 9.8.6"; do
  read -r setting ours theirs <<<"$mismatch"
  new_case "ios: an Xcode project whose $setting disagrees with project.yml stops before building"
  pbx=$c/repo/ios-pos/FedimintCardPOS.xcodeproj/project.pbxproj
  # One configuration keeps another value, as when project.yml changed and xcodegen did not run.
  awk -v from="$setting = $ours;" -v to="$setting = $theirs;" \
    '!done && (i = index($0, from)) { $0 = substr($0, 1, i - 1) to substr($0, i + length(from)); done = 1 } { print }' \
    "$pbx" >"$pbx.new"
  mv "$pbx.new" "$pbx"
  expect "fixture: one configuration says $theirs" is "$(grep -cF "$setting = $theirs;" "$pbx")" 1
  ios "${asc[@]}"
  expect "fails" is "$status" 1
  expect "says to regenerate the project" \
    has "$c/err" "$setting differs from ios-pos/project.yml ($ours): run xcodegen generate"
  expect "builds nothing" empty "$c/log"
done

new_case "ios: a project.yml without MARKETING_VERSION stops before building"
grep -v 'MARKETING_VERSION:' "$c/repo/ios-pos/project.yml" >"$c/project.yml"
mv "$c/project.yml" "$c/repo/ios-pos/project.yml"
ios "${asc[@]}"
expect "fails" is "$status" 1
expect "names the setting" has "$c/err" "ios-pos/project.yml must set MARKETING_VERSION once"
expect "builds nothing" empty "$c/log"

echo
if ((failed)); then
  echo "$failed failed, in $cases cases" >&2
  exit 1
fi
echo "all passed, $cases cases"
