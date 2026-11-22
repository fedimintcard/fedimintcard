#!/usr/bin/env bash
# Tests for the release scripts (docs/RELEASING.md): scripts/release-android.sh,
# ios-pos/scripts/release-ios.sh and apps/provisioner/scripts/release-ios.sh. Each case copies a
# script into a scratch tree and runs it with an environment of its own (env -i), a scratch HOME and
# stand-ins for Gradle, xcodebuild, the core build, the Keychain, node, yarn and CocoaPods that
# record their calls. Nothing is built, signed or uploaded, and the real Keychain is never read. The
# fixtures take this checkout's project files with test versions. PATH holds the stand-ins and the
# system directories only, so on macOS the scripts run under /bin/bash 3.2 and the BSD tools, as
# they do on the release Mac.
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

# --- apps/provisioner/scripts/release-ios.sh (the Setup app) ---------------------------------
#
# The Setup script builds only from a checkout git calls clean, so each of its cases runs in a
# scratch git checkout, committed with no user or system git configuration and no signing. The
# stand-ins: node answers --version and runs the two release gates; yarn copies tools/cardkit on
# install, as yarn copies a file: dependency, and leaves a copy that is already there alone; pod
# writes the files pod writes, which git ignores; xcodebuild writes an archive in Xcode's layout, or
# exports and prints a marker in place of an upload. pod and xcodebuild can also change a tracked
# file or make a commit while they run, as other work in the same checkout could.

setup_id=org.fedimintcard.provisioner
setup_version=9.8.7 setup_build=65 # what the fixture's package.json says
setup_archive=dist/ios/FedimintProvisioner-$setup_version-$setup_build.xcarchive
setup_commit=${setup_archive%.xcarchive}.commit
origin_production=https://registry.test.invalid # the registry origins the stand-in gate prints
origin_staging=https://staging-registry.test.invalid
registry_lines="  registry test: $origin_staging
  registry production: $origin_production"
load_hash=0123456789abcdeffedcba98765432100123456789abcdeffedcba9876543210 # the fixture's sha256
load_short=0123456789abcdef                                                  # its first 16 hex
bundle_with() { # strings...: a stand-in main.jsbundle body holding each of them
  local s body=
  for s in "$@"; do body="${body}var s=\"$s\";"; done
  echo "$body"
}
bundle_ok=$(bundle_with "$origin_production" "$origin_staging" "$load_hash")
built_commit=5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e # what an upload case's .commit says
fake_archive_tool=$scratch/fake-archive
cat >"$fake_archive_tool" <<'EOF'
#!/usr/bin/env bash
# dir id version build [uploaded]: an .xcarchive with its Info.plist in Xcode's layout and the app's
# directory; with uploaded, the record Xcode adds to it when it uploads that build.
set -euo pipefail
dir=$1 id=$2 version=$3 build=$4 uploaded=${5:-}
mkdir -p "$dir/Products/Applications/FedimintProvisioner.app"
{
  printf '<?xml version="1.0" encoding="UTF-8"?>\n'
  printf '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">\n'
  printf '<plist version="1.0">\n<dict>\n\t<key>ApplicationProperties</key>\n\t<dict>\n'
  printf '\t\t<key>ApplicationPath</key>\n\t\t<string>Applications/FedimintProvisioner.app</string>\n'
  printf '\t\t<key>CFBundleIdentifier</key>\n\t\t<string>%s</string>\n' "$id"
  printf '\t\t<key>CFBundleShortVersionString</key>\n\t\t<string>%s</string>\n' "$version"
  printf '\t\t<key>CFBundleVersion</key>\n\t\t<string>%s</string>\n' "$build"
  printf '\t\t<key>Team</key>\n\t\t<string>H7UAM79QQP</string>\n\t</dict>\n'
  if [[ -n $uploaded ]]; then
    printf '\t<key>Distributions</key>\n\t<array>\n\t\t<dict>\n'
    printf '\t\t\t<key>destination</key>\n\t\t\t<string>upload</string>\n'
    printf '\t\t\t<key>uploadedBuildNumber</key>\n\t\t\t<string>%s</string>\n' "$uploaded"
    printf '\t\t</dict>\n\t</array>\n'
  fi
  printf '\t<key>Name</key>\n\t<string>FedimintProvisioner</string>\n</dict>\n</plist>\n'
} >"$dir/Info.plist"
EOF
chmod +x "$fake_archive_tool"
fake_archive() { "$fake_archive_tool" "$@"; } # dir id version build [uploaded]
fixture_git() { # git in the scratch checkout, with no user or system configuration and no signing
  GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 git -C "$c/repo" -c commit.gpgsign=false \
    -c user.name=fixture -c user.email=fixture@example.invalid "$@"
}
# An empty commit from inside a stand-in, where fixture_git is not defined, with the same
# configuration: other work in the checkout while the build runs.
fixture_commit='GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 git -c commit.gpgsign=false -c user.name=fixture -c user.email=fixture@example.invalid commit -q --allow-empty -m "a commit while the build runs"'
# shellcheck disable=SC2016 # the stand-ins' bodies expand their variables when they run
new_setup_case() { # name: a scratch git checkout in $c with the Setup script, its inputs and stand-ins
  cases=$((cases + 1))
  c=$scratch/$cases
  echo "# $1"
  local app=$c/repo/apps/provisioner
  mkdir -p "$app/scripts" "$app/ios" "$c/repo/tools/cardkit/gp" "$c/home" "$c/keys"
  : >"$c/log"
  cp "$repo/apps/provisioner/scripts/release-ios.sh" "$app/scripts/"
  cp "$repo/apps/provisioner/ios/ExportOptions.plist" "$app/ios/"
  cp "$repo/.gitignore" "$c/repo/"
  cp "$repo/apps/provisioner/.gitignore" "$app/"
  fixture "$repo/apps/provisioner/package.json" "$app/package.json" \
    's/^(  "version": )"[^"]+",$/\1"9.8.7",/; s/^(  "provisionerBuild": )[0-9]+,$/\165,/' \
    '^  "version": "9\.8\.7",$' '^  "provisionerBuild": 65,$'
  fixture "$repo/apps/provisioner/ios/FedimintProvisioner.xcodeproj/project.pbxproj" \
    "$app/ios/FedimintProvisioner.xcodeproj/project.pbxproj" \
    's/^([[:space:]]+MARKETING_VERSION = ).*;$/\19.8.7;/; s/^([[:space:]]+CURRENT_PROJECT_VERSION = ).*;$/\165;/' \
    '^[[:space:]]+MARKETING_VERSION = 9\.8\.7;$' '^[[:space:]]+CURRENT_PROJECT_VERSION = 65;$'
  cp "$repo/tools/cardkit/index.ts" "$c/repo/tools/cardkit/"
  cp "$repo/tools/cardkit/gp/apdu.ts" "$c/repo/tools/cardkit/gp/"
  fixture "$repo/tools/cardkit/gp/load-file.ts" "$c/repo/tools/cardkit/gp/load-file.ts" \
    "s/^(  sha256: ')[0-9a-f]{64}(',)\$/\\1$load_hash\\2/" "^  sha256: '$load_hash',\$"
  # node: --version leaves no line in the log; the two gates print what the real ones print.
  stand_in "$c/bin/node" 'if [[ $* == --version ]]; then echo "$FAKE_NODE_VERSION"; exit 0; fi
echo "node $* (in $PWD)" >>"$FAKE_LOG"
case $* in
  "scripts/sync-version.cjs --check")
    if [[ -n ${FAKE_SYNC_FAILS:-} ]]; then echo "sync-version: android/app/build.gradle: versionCode is not 65" >&2; exit 1; fi
    echo "sync-version: 9.8.7 (65) is consistent" ;;
  "scripts/configure-service.cjs --check --production")
    echo "  local session: disabled"
    if [[ -n ${FAKE_CONFIG_FAILS:-} ]]; then echo "configure-service: LOCAL_SESSION_ENABLED is true" >&2; exit 1; fi
    echo "$FAKE_REGISTRY_LINES"
    echo "configure-service: production configuration OK" ;;
  *) echo "node stand-in: unexpected node $*" >&2; exit 99 ;;
esac'
  # yarn: records whether the cardkit copy is there when it starts. install copies tools/cardkit
  # when there is no copy, as yarn copies a file: dependency, and FAKE_STALE_CARDKIT spoils it.
  stand_in "$c/bin/yarn" 'copy=node_modules/@fedimintcard/cardkit
there=absent; if [[ -e $copy ]]; then there=present; fi
echo "yarn $* (in $PWD) cardkit=$there" >>"$FAKE_LOG"
case $1 in
  install)
    if [[ ! -e $copy ]]; then mkdir -p "$(dirname "$copy")" && cp -R ../../tools/cardkit "$copy"; fi
    case ${FAKE_STALE_CARDKIT:-} in
      changed) echo "// an older cardkit" >>"$copy/gp/apdu.ts" ;;
      missing) rm "$copy/gp/load-file.ts" ;;
      extra) echo "// a file cardkit no longer has" >"$copy/gp/extra.ts" ;;
    esac
    exit "${FAKE_YARN_INSTALL_STATUS:-0}" ;;
  test) exit "${FAKE_YARN_TEST_STATUS:-0}" ;;
esac
echo "yarn stand-in: unexpected yarn $*" >&2; exit 99'
  # pod: writes its ignored files; FAKE_POD_REWRITES names a tracked file it rewrites as well, and
  # FAKE_POD_COMMITS makes a commit while it runs.
  stand_in "$c/bin/pod" 'echo "pod $* (in $PWD)" >>"$FAKE_LOG"
mkdir -p Pods && echo pods >Pods/Manifest.lock && echo pods >Podfile.lock
if [[ -n ${FAKE_POD_REWRITES:-} ]]; then echo "// pod install" >>"$FAKE_POD_REWRITES"; fi
if [[ -n ${FAKE_POD_COMMITS:-} ]]; then '"$fixture_commit"'; fi
exit "${FAKE_POD_STATUS:-0}"'
  # xcodebuild: archive writes the archive FAKE_* describe, and FAKE_ARCHIVE_TOUCHES (a tracked file
  # it appends to) or FAKE_ARCHIVE_COMMITS changes the checkout while it runs; -exportArchive prints
  # a marker where the real one uploads and writes nothing, as an upload leaves no export folder, or
  # fails with FAKE_EXPORT_STATUS.
  stand_in "$c/bin/xcodebuild" 'echo "xcodebuild $* (in $PWD)" >>"$FAKE_LOG"
archive= export_path=
while (($#)); do
  case $1 in -archivePath) archive=$2; shift ;; -exportPath) export_path=$2; shift ;; esac
  shift
done
if [[ -n $export_path ]]; then
  if [[ -n ${FAKE_EXPORT_STATUS:-} ]]; then exit "$FAKE_EXPORT_STATUS"; fi
  echo "xcodebuild stand-in: EXPORTED AND UPLOADED"
  exit 0
fi
if [[ -n ${FAKE_XCODEBUILD_STATUS:-} ]]; then exit "$FAKE_XCODEBUILD_STATUS"; fi
if [[ -n ${FAKE_NO_ARCHIVE:-} ]]; then exit 0; fi
"$FAKE_ARCHIVE_TOOL" "$archive" "$FAKE_BUNDLE_ID" "$FAKE_ARCHIVE_VERSION" "$FAKE_ARCHIVE_BUILD"
if [[ -z ${FAKE_NO_JSBUNDLE:-} ]]; then
  printf "HBC\0%s\0" "$FAKE_JSBUNDLE" >"$archive/Products/Applications/FedimintProvisioner.app/main.jsbundle"
fi
if [[ -n ${FAKE_ARCHIVE_TOUCHES:-} ]]; then echo "// changed while archiving" >>"$FAKE_ARCHIVE_TOUCHES"; fi
if [[ -n ${FAKE_ARCHIVE_COMMITS:-} ]]; then '"$fixture_commit"'; fi'
  echo "not a key" >"$c/keys/AuthKey_TEST.p8"
  fixture_git -c init.defaultBranch=main init -q
  fixture_git add -A
  fixture_git commit -qm fixture
}
setup_ios() { # [NAME=value ...] [-- args]: the Setup script, with the stand-ins' defaults first
  run apps/provisioner/scripts/release-ios.sh FAKE_ARCHIVE_TOOL="$fake_archive_tool" \
    FAKE_NODE_VERSION=v22.12.0 FAKE_REGISTRY_LINES="$registry_lines" FAKE_BUNDLE_ID=$setup_id \
    FAKE_ARCHIVE_VERSION=$setup_version FAKE_ARCHIVE_BUILD=$setup_build FAKE_JSBUNDLE="$bundle_ok" "$@"
}
setup_built() { # [uploaded]: the archive a build leaves in the scratch checkout, with its .commit
  fake_archive "$c/repo/$setup_archive" "$setup_id" "$setup_version" "$setup_build" "$@"
  printf 'HBC\0%s\0' "$bundle_ok" >"$c/repo/$setup_archive/Products/Applications/FedimintProvisioner.app/main.jsbundle"
  echo "$built_commit" >"$c/repo/$setup_commit"
}
prints_no_asc_value() { # the run's output names no key path, key ID or issuer ID
  cat "$c/out" "$c/err" >"$c/all"
  expect "prints no key path" lacks "$c/all" "AuthKey_TEST"
  expect "prints no key ID" lacks "$c/all" "TESTKEYID"
  expect "prints no issuer ID" lacks "$c/all" "test-issuer-id"
}
line_of() { grep -nF -m 1 -- "$2" "$1" | cut -d: -f1 || true; } # file text: its first line number

usage_runs=("no subcommand|" "an unknown subcommand|archive" "upload without an archive|upload"
  "build with an argument|build now" "upload with two archives|upload a b")
for entry in "${usage_runs[@]}"; do
  new_setup_case "setup ios: ${entry%%|*} is refused with the usage, before the key check"
  # shellcheck disable=SC2086 # the words after | are the arguments
  setup_ios -- ${entry#*|}
  expect "fails with usage" is "$status" 2
  expect "prints usage" is "$(cat "$c/err")" "usage: apps/provisioner/scripts/release-ios.sh build | upload <archive>"
  expect "runs nothing" empty "$c/log"
done

for sub in build upload; do
  sub_args=(build)
  if [[ $sub == upload ]]; then sub_args=(upload "repo/$setup_archive"); fi
  new_setup_case "setup ios $sub: no App Store Connect key stops it before anything runs"
  if [[ $sub == upload ]]; then setup_built; fi
  setup_ios -- "${sub_args[@]}"
  expect "fails" is "$status" 1
  expect "names all three variables" is "$(cat "$c/err")" \
    "set ASC_KEY_PATH ASC_KEY_ID ASC_ISSUER_ID (an App Store Connect API key) to archive and upload"
  expect "runs nothing" empty "$c/log"
  for i in 0 1 2; do
    name=${asc[$i]%%=*}
    others=()
    for j in 0 1 2; do if ((j != i)); then others+=("${asc[$j]}"); fi; done
    new_setup_case "setup ios $sub: without $name it stops before anything runs"
    if [[ $sub == upload ]]; then setup_built; fi
    setup_ios "${others[@]}" -- "${sub_args[@]}"
    expect "fails" is "$status" 1
    expect "names $name alone" is "$(cat "$c/err")" "set $name (an App Store Connect API key) to archive and upload"
    expect "runs nothing" empty "$c/log"
  done
  new_setup_case "setup ios $sub: a key path naming no file stops it, without printing the values"
  if [[ $sub == upload ]]; then setup_built; fi
  setup_ios ASC_KEY_PATH="$c/keys/AuthKey_MISSING.p8" ASC_KEY_ID=TESTKEYID ASC_ISSUER_ID=test-issuer-id \
    -- "${sub_args[@]}"
  expect "fails" is "$status" 1
  expect "names ASC_KEY_PATH" is "$(cat "$c/err")" "ASC_KEY_PATH names no readable key file"
  expect "runs nothing" empty "$c/log"
  expect "prints no key path" lacks "$c/out" "AuthKey_MISSING"
  prints_no_asc_value
done

new_setup_case "setup ios build: a modified tracked file stops it before anything runs"
echo "// edited" >>"$c/repo/tools/cardkit/index.ts"
setup_ios "${asc[@]}" -- build
expect "fails" is "$status" 1
expect "says the tree is not clean" \
  has "$c/err" "the working tree is not clean (git status --short): commit, stash or remove these first"
expect "names the file" has "$c/err" "tools/cardkit/index.ts"
expect "runs nothing" empty "$c/log"
expect "writes nothing to dist" test ! -e "$c/repo/dist"

new_setup_case "setup ios build: an untracked file stops it, where git status hides untracked files"
fixture_git config status.showUntrackedFiles no
echo "notes" >"$c/repo/apps/provisioner/notes.txt"
setup_ios "${asc[@]}" -- build
expect "fails" is "$status" 1
expect "says the tree is not clean" has "$c/err" "the working tree is not clean"
expect "names the file" has "$c/err" "apps/provisioner/notes.txt"
expect "runs nothing" empty "$c/log"

new_setup_case "setup ios build: a tree that is not a git checkout stops it before anything runs"
rm -rf "$c/repo/.git"
# git looks for a repository no higher than the scratch tree, wherever the temporary directory is
setup_ios "${asc[@]}" GIT_CEILING_DIRECTORIES="$c" -- build
expect "fails" is "$status" 1
expect "says it is not a checkout" has "$c/err" "$c/repo is not a git checkout"
expect "runs nothing" empty "$c/log"

new_setup_case "setup ios build: SKIP_BUNDLING stops it before anything runs"
setup_ios "${asc[@]}" SKIP_BUNDLING=1 -- build
expect "fails" is "$status" 1
expect "says the archive would have no bundle" \
  is "$(cat "$c/err")" "SKIP_BUNDLING is set: the archive would carry no JS bundle"
expect "runs nothing" empty "$c/log"

for existing in "$setup_archive" "$setup_commit"; do
  new_setup_case "setup ios build: an existing ${existing##*/} stops it before anything runs, untouched"
  mkdir -p "$c/repo/$(dirname "$existing")"
  if [[ $existing == *.xcarchive ]]; then
    mkdir "$c/repo/$existing" && echo "an earlier archive" >"$c/repo/$existing/marker"
  else
    echo "an earlier commit" >"$c/repo/$existing"
  fi
  setup_ios "${asc[@]}" -- build
  expect "fails" is "$status" 1
  expect "says to move it or bump the build" \
    is "$(cat "$c/err")" "$c/repo/$existing exists: move it aside or bump provisionerBuild"
  expect "runs nothing" empty "$c/log"
  if [[ $existing == *.xcarchive ]]; then
    expect "leaves the archive alone" test -f "$c/repo/$existing/marker"
    expect "records no commit" test ! -e "$c/repo/$setup_commit"
  else
    expect "leaves the commit alone" is "$(cat "$c/repo/$existing")" "an earlier commit"
    expect "writes no archive" test ! -e "$c/repo/$setup_archive"
  fi
done

new_setup_case "setup ios build: node 20 stops it before the release gates"
setup_ios "${asc[@]}" FAKE_NODE_VERSION=v20.18.0 -- build
expect "fails" is "$status" 1
expect "names the version it needs and found" \
  is "$(cat "$c/err")" "node 22 or newer is required (apps/provisioner/package.json engines); found v20.18.0"
expect "runs nothing" empty "$c/log"

new_setup_case "setup ios build: package.json without provisionerBuild stops it before anything runs"
grep -v '"provisionerBuild"' "$c/repo/apps/provisioner/package.json" >"$c/package.json"
mv "$c/package.json" "$c/repo/apps/provisioner/package.json"
fixture_git commit -qam "no provisionerBuild"
setup_ios "${asc[@]}" -- build
expect "fails" is "$status" 1
expect "names the field" has "$c/err" "apps/provisioner/package.json must set provisionerBuild once"
expect "runs nothing" empty "$c/log"

new_setup_case "setup ios build: a failing version gate stops it before yarn, pod and xcodebuild"
setup_ios "${asc[@]}" FAKE_SYNC_FAILS=1 -- build
expect "fails" is "$status" 1
expect "passes the gate's own message on" has "$c/err" "sync-version: android/app/build.gradle: versionCode is not 65"
expect "names the gate" has "$c/err" "release gate failed: node scripts/sync-version.cjs --check"
expect "runs only that gate" is "$(cat "$c/log")" "node scripts/sync-version.cjs --check (in $c/repo/apps/provisioner)"

new_setup_case "setup ios build: a failing configuration gate stops it before yarn, pod and xcodebuild"
setup_ios "${asc[@]}" FAKE_CONFIG_FAILS=1 -- build
expect "fails" is "$status" 1
expect "passes the gate's own message on" has "$c/err" "configure-service: LOCAL_SESSION_ENABLED is true"
expect "names the gate" has "$c/err" "release gate failed: node scripts/configure-service.cjs --check --production"
expect "runs only the two gates" is "$(wc -l <"$c/log" | tr -d ' ')" 2
expect "runs no yarn" lacks "$c/log" "yarn "

new_setup_case "setup ios build: a configuration gate that prints no registry origin stops it before yarn"
setup_ios "${asc[@]}" FAKE_REGISTRY_LINES="  production registry at $origin_production" -- build
expect "fails" is "$status" 1
expect "says the gate's output changed" has "$c/err" "configure-service.cjs printed no registry origin"
expect "runs only the two gates" is "$(wc -l <"$c/log" | tr -d ' ')" 2

new_setup_case "setup ios build: a load-file.ts without its sha256 stops it before yarn"
grep -v "sha256:" "$c/repo/tools/cardkit/gp/load-file.ts" >"$c/load-file.ts"
mv "$c/load-file.ts" "$c/repo/tools/cardkit/gp/load-file.ts"
fixture_git commit -qam "no sha256"
setup_ios "${asc[@]}" -- build
expect "fails" is "$status" 1
expect "says there is no hash" is "$(cat "$c/err")" "no load file hash in tools/cardkit/gp/load-file.ts"
expect "runs only the two gates" is "$(wc -l <"$c/log" | tr -d ' ')" 2

new_setup_case "setup ios build: from a clean checkout, refreshes cardkit, tests, archives, checks and records the commit"
# The copy an earlier install left, which the build must replace.
mkdir -p "$c/repo/apps/provisioner/node_modules/@fedimintcard/cardkit/gp"
echo "// an older cardkit" >"$c/repo/apps/provisioner/node_modules/@fedimintcard/cardkit/gp/apdu.ts"
head_commit=$(fixture_git rev-parse HEAD)
setup_ios "${asc[@]}" -- build
app=$c/repo/apps/provisioner
expect "succeeds" is "$status" 0
expect "prints the commit before it builds" is "$(sed -n 1p "$c/out")" "commit $head_commit"
expect "runs the version gate first, in apps/provisioner" \
  is "$(sed -n 1p "$c/log")" "node scripts/sync-version.cjs --check (in $app)"
expect "then the configuration gate" \
  is "$(sed -n 2p "$c/log")" "node scripts/configure-service.cjs --check --production (in $app)"
expect "then installs, with the old cardkit copy removed first" \
  is "$(sed -n 3p "$c/log")" "yarn install --frozen-lockfile --check-files (in $app) cardkit=absent"
expect "then runs the unit tests" is "$(sed -n 4p "$c/log")" "yarn test (in $app) cardkit=present"
expect "then installs the pods" is "$(sed -n 5p "$c/log")" "pod install (in $app/ios)"
expect "then archives the workspace, signing with the API key" is "$(sed -n 6p "$c/log")" \
  "xcodebuild -workspace apps/provisioner/ios/FedimintProvisioner.xcworkspace -scheme FedimintProvisioner -configuration Release -destination generic/platform=iOS -allowProvisioningUpdates -authenticationKeyPath $c/keys/AuthKey_TEST.p8 -authenticationKeyID TESTKEYID -authenticationKeyIssuerID test-issuer-id archive -archivePath $setup_archive (in $c/repo)"
expect "runs nothing else" is "$(wc -l <"$c/log" | tr -d ' ')" 6
expect "never exports or uploads" lacks "$c/log" "-exportArchive"
expect "records the commit beside the archive" is "$(cat "$c/repo/$setup_commit")" "$head_commit"
expect "prints the bundle id" has "$c/out" "bundle id: $setup_id"
expect "prints the version and build" has "$c/out" "version: 9.8.7 (65)"
expect "prints the commit" has "$c/out" "commit: $head_commit"
expect "prints the team" has "$c/out" "team: H7UAM79QQP"
expect "prints what the bundle holds" is "$(grep '^main.jsbundle holds ' "$c/out")" \
  "main.jsbundle holds $origin_staging, $origin_production and the load file hash $load_short"
expect "prints the upload step" has "$c/out" "next: apps/provisioner/scripts/release-ios.sh upload $c/repo/$setup_archive"
expect "last line is the archive's path" is "$(last_line "$c/out")" "$c/repo/$setup_archive"
expect "the tree is still clean" is "$(fixture_git status --porcelain --untracked-files=normal)" ""
prints_no_asc_value

for stale in changed:gp/apdu.ts missing:gp/load-file.ts extra:gp/extra.ts; do
  new_setup_case "setup ios build: an installed cardkit that differs from tools/cardkit (${stale%%:*}) stops it before yarn test"
  setup_ios "${asc[@]}" FAKE_STALE_CARDKIT="${stale%%:*}" -- build
  expect "fails" is "$status" 1
  expect "names the file" is "$(cat "$c/err")" "the installed cardkit differs from tools/cardkit: ${stale#*:}"
  expect "runs no yarn test, pod or xcodebuild" is "$(sed -n '$p' "$c/log")" \
    "yarn install --frozen-lockfile --check-files (in $c/repo/apps/provisioner) cardkit=absent"
done

new_setup_case "setup ios build: a failing yarn install stops it with yarn's status"
setup_ios "${asc[@]}" FAKE_YARN_INSTALL_STATUS=5 -- build
expect "fails with yarn's status" is "$status" 5
expect "runs no yarn test, pod or xcodebuild" lacks "$c/log" "yarn test"
expect "archives nothing" lacks "$c/log" "xcodebuild"

new_setup_case "setup ios build: red unit tests stop it before pod install"
setup_ios "${asc[@]}" FAKE_YARN_TEST_STATUS=1 -- build
expect "fails" is "$status" 1
expect "says the tests failed" has "$c/err" "yarn test failed"
expect "runs no pod or xcodebuild" is "$(sed -n '$p' "$c/log")" "yarn test (in $c/repo/apps/provisioner) cardkit=present"

new_setup_case "setup ios build: pod install rewriting a tracked file stops it before xcodebuild"
setup_ios "${asc[@]}" FAKE_POD_REWRITES=FedimintProvisioner.xcodeproj/project.pbxproj -- build
expect "fails" is "$status" 1
expect "says the tree changed" has "$c/err" "the tree changed while preparing"
expect "names the file" has "$c/err" "apps/provisioner/ios/FedimintProvisioner.xcodeproj/project.pbxproj"
expect "archives nothing" lacks "$c/log" "xcodebuild"

new_setup_case "setup ios build: a commit made while preparing stops it before xcodebuild"
head_commit=$(fixture_git rev-parse HEAD)
setup_ios "${asc[@]}" FAKE_POD_COMMITS=1 -- build
expect "fails" is "$status" 1
expect "says HEAD moved" has "$c/err" "HEAD moved while preparing: the archive would not be $head_commit"
expect "the commit was made" test "$(fixture_git rev-parse HEAD)" != "$head_commit"
expect "archives nothing" lacks "$c/log" "xcodebuild"

new_setup_case "setup ios build: a failing pod install stops it with pod's status"
setup_ios "${asc[@]}" FAKE_POD_STATUS=7 -- build
expect "fails with pod's status" is "$status" 7
expect "archives nothing" lacks "$c/log" "xcodebuild"

new_setup_case "setup ios build: a failed archive passes xcodebuild's status on and records no commit"
setup_ios "${asc[@]}" FAKE_XCODEBUILD_STATUS=65 -- build
expect "fails with xcodebuild's status" is "$status" 65
expect "archived once" is "$(grep -c '^xcodebuild ' "$c/log")" 1
expect "records no commit" test ! -e "$c/repo/$setup_commit"

new_setup_case "setup ios build: an archive xcodebuild did not write stops it"
setup_ios "${asc[@]}" FAKE_NO_ARCHIVE=1 -- build
expect "fails" is "$status" 1
expect "says there is no archive" has "$c/err" "xcodebuild wrote no archive at $c/repo/$setup_archive"
expect "records no commit" test ! -e "$c/repo/$setup_commit"

archive_refusals=(
  "the POS app's bundle id|FAKE_BUNDLE_ID=org.fedimintcard.pos|the archive is for org.fedimintcard.pos, not $setup_id"
  "another version|FAKE_ARCHIVE_VERSION=9.8.6|the archive says 9.8.6 (65), package.json says 9.8.7 (65)"
  "another build|FAKE_ARCHIVE_BUILD=64|the archive says 9.8.7 (64), package.json says 9.8.7 (65)"
  "no main.jsbundle|FAKE_NO_JSBUNDLE=1|the app has no main.jsbundle"
  "a bundle without the production registry|FAKE_JSBUNDLE=$(bundle_with "$origin_staging" "$load_hash")|main.jsbundle does not hold the registry origin $origin_production"
  "a bundle without the staging registry|FAKE_JSBUNDLE=$(bundle_with "$origin_production" "$load_hash")|main.jsbundle does not hold the registry origin $origin_staging"
  "a bundle without the load file hash|FAKE_JSBUNDLE=$(bundle_with "$origin_production" "$origin_staging")|main.jsbundle does not hold the load file hash $load_short"
)
for entry in "${archive_refusals[@]}"; do
  what=${entry%%|*} rest=${entry#*|}
  new_setup_case "setup ios build: an archive with $what is refused, with no commit recorded"
  setup_ios "${asc[@]}" "${rest%%|*}" -- build
  expect "fails" is "$status" 1
  expect "says why" is "$(cat "$c/err")" "${rest#*|}"
  expect "archived once" is "$(grep -c '^xcodebuild ' "$c/log")" 1
  expect "records no commit" test ! -e "$c/repo/$setup_commit"
done

new_setup_case "setup ios build: a tracked file changed while archiving stops it, with no commit recorded"
head_commit=$(fixture_git rev-parse HEAD)
setup_ios "${asc[@]}" FAKE_ARCHIVE_TOUCHES=apps/provisioner/package.json -- build
expect "fails" is "$status" 1
expect "says the archive may not be the commit" \
  has "$c/err" "the tree or HEAD changed while archiving: the archive may not be $head_commit"
expect "names the file" has "$c/err" "apps/provisioner/package.json"
expect "archived once" is "$(grep -c '^xcodebuild ' "$c/log")" 1
expect "records no commit" test ! -e "$c/repo/$setup_commit"
expect "prints no upload step" lacks "$c/out" "next: "

new_setup_case "setup ios build: a commit made while archiving stops it, with no commit recorded"
head_commit=$(fixture_git rev-parse HEAD)
setup_ios "${asc[@]}" FAKE_ARCHIVE_COMMITS=1 -- build
expect "fails" is "$status" 1
expect "says the archive may not be the commit" \
  has "$c/err" "the tree or HEAD changed while archiving: the archive may not be $head_commit"
expect "says where HEAD is now" has "$c/err" "HEAD is $(fixture_git rev-parse HEAD) now"
expect "records no commit" test ! -e "$c/repo/$setup_commit"
expect "prints no upload step" lacks "$c/out" "next: "

# SignInScreen imports the local session's label, so every Release bundle holds it whatever
# LOCAL_SESSION_ENABLED says: Hermes stores it as UTF-16 for its em-dash, and as plain bytes without
# one. The configuration gate keeps the session out of a release, so no bundle check looks for it.
new_setup_case "setup ios build: a bundle holding the local session's label, as every bundle does, is accepted"
setup_ios "${asc[@]}" \
  FAKE_JSBUNDLE="$(bundle_with "$origin_production" "$origin_staging" "$load_hash" "LOCAL SESSION - nothing is sent to a registry")" \
  -- build
expect "succeeds" is "$status" 0
expect "records the commit" test -f "$c/repo/$setup_commit"
expect "claims nothing about the local session" lacks "$c/out" "LOCAL SESSION"

new_setup_case "setup ios upload: a path with nothing there is refused before xcodebuild"
setup_ios "${asc[@]}" -- upload repo/dist/ios/FedimintProvisioner-9.8.7-66.xcarchive
expect "fails" is "$status" 1
expect "says build did not make it" \
  is "$(cat "$c/err")" "$c/repo/dist/ios/FedimintProvisioner-9.8.7-66.xcarchive was not made by release-ios.sh build"
expect "uploads nothing" empty "$c/log"

new_setup_case "setup ios upload: a directory whose name does not end in .xcarchive is refused"
fake_archive "$c/repo/dist/ios/FedimintProvisioner-9.8.7-65" "$setup_id" 9.8.7 65
echo "$built_commit" >"$c/repo/dist/ios/FedimintProvisioner-9.8.7-65.commit"
setup_ios "${asc[@]}" -- upload repo/dist/ios/FedimintProvisioner-9.8.7-65
expect "fails" is "$status" 1
expect "says build did not make it" has "$c/err" "FedimintProvisioner-9.8.7-65 was not made by release-ios.sh build"
expect "uploads nothing" empty "$c/log"

new_setup_case "setup ios upload: an archive without the .commit a build writes is refused"
fake_archive "$c/repo/$setup_archive" "$setup_id" 9.8.7 65
setup_ios "${asc[@]}" -- upload "repo/$setup_archive"
expect "fails" is "$status" 1
expect "says build did not make it" is "$(cat "$c/err")" "$c/repo/$setup_archive was not made by release-ios.sh build"
expect "uploads nothing" empty "$c/log"

new_setup_case "setup ios upload: an .xcarchive with no Info.plist is refused"
mkdir -p "$c/repo/$setup_archive" && echo "$built_commit" >"$c/repo/$setup_commit"
setup_ios "${asc[@]}" -- upload "repo/$setup_archive"
expect "fails" is "$status" 1
expect "says build did not make it" is "$(cat "$c/err")" "$c/repo/$setup_archive was not made by release-ios.sh build"
expect "uploads nothing" empty "$c/log"

new_setup_case "setup ios upload: an archive of the POS app is refused"
pos_archive=dist/ios/FedimintCardPOS-9.8.7-65.xcarchive
fake_archive "$c/repo/$pos_archive" org.fedimintcard.pos 9.8.7 65
echo "$built_commit" >"$c/repo/${pos_archive%.xcarchive}.commit"
setup_ios "${asc[@]}" -- upload "repo/$pos_archive"
expect "fails" is "$status" 1
expect "names the app it is for" is "$(cat "$c/err")" "the archive is for org.fedimintcard.pos, not $setup_id"
expect "uploads nothing" empty "$c/log"

new_setup_case "setup ios upload: an archive that records an upload of its build is refused"
setup_built 65
setup_ios "${asc[@]}" -- upload "repo/$setup_archive"
expect "fails" is "$status" 1
expect "says it was uploaded" is "$(cat "$c/err")" "$c/repo/$setup_archive already records an upload of build 65"
expect "uploads nothing" empty "$c/log"

new_setup_case "setup ios upload: says what it will upload, then exports and uploads that archive alone"
setup_built
# The path as the caller's shell completes it: relative to where the caller is, with a slash.
setup_ios "${asc[@]}" -- upload "repo/$setup_archive/"
expect "succeeds" is "$status" 0
will=$(line_of "$c/out" "will upload $setup_id 9.8.7 (65), built from $built_commit, to App Store Connect (TestFlight)")
uploaded=$(line_of "$c/out" "xcodebuild stand-in: EXPORTED AND UPLOADED")
expect "says what it will upload" test -n "$will"
expect "says it before the upload" test "${will:-999}" -lt "${uploaded:-0}"
expect "exports and uploads that archive with the API key, and runs nothing else" is "$(cat "$c/log")" \
  "xcodebuild -exportArchive -archivePath $c/repo/$setup_archive -exportOptionsPlist apps/provisioner/ios/ExportOptions.plist -exportPath dist/ios/export-FedimintProvisioner-9.8.7-65 -allowProvisioningUpdates -authenticationKeyPath $c/keys/AuthKey_TEST.p8 -authenticationKeyID TESTKEYID -authenticationKeyIssuerID test-issuer-id (in $c/repo)"
expect "last line is the archive it uploaded" is "$(last_line "$c/out")" "$c/repo/$setup_archive"
expect "leaves nothing at the export path" \
  test ! -e "$c/repo/dist/ios/export-FedimintProvisioner-9.8.7-65"
prints_no_asc_value

new_setup_case "setup ios upload: a failed export passes xcodebuild's status on and prints no archive path"
setup_built
setup_ios "${asc[@]}" FAKE_EXPORT_STATUS=70 -- upload "repo/$setup_archive"
expect "fails with xcodebuild's status" is "$status" 70
expect "said what it would upload" has "$c/out" "will upload $setup_id 9.8.7 (65)"
expect "prints no archive path" lacks "$c/out" "$c/repo/$setup_archive"
expect "uploads nothing" lacks "$c/out" "EXPORTED AND UPLOADED"

cases=$((cases + 1))
echo "# setup ios: ExportOptions.plist uploads to App Store Connect for the team, with the archive's numbers"
plist_value() { # key plist: the value after <key>key</key>
  sed -nE '/<key>'"$1"'<\/key>/{n;s/^[[:space:]]*<string>([^<]*)<\/string>[[:space:]]*$/\1/p;s/^[[:space:]]*<(true|false)\/>[[:space:]]*$/\1/p;}' "$2"
}
export_options=$repo/apps/provisioner/ios/ExportOptions.plist
expect "method app-store-connect" is "$(plist_value method "$export_options")" app-store-connect
expect "destination upload" is "$(plist_value destination "$export_options")" upload
expect "team H7UAM79QQP" is "$(plist_value teamID "$export_options")" H7UAM79QQP
expect "keeps the archive's version and build" is "$(plist_value manageAppVersionAndBuildNumber "$export_options")" false

# --- the version numbers in this checkout ----------------------------------------------------
#
# No script runs and no scratch tree is made: this case reads this checkout's own project files,
# with the release scripts' own sed expressions where a script reads the file. Each app's projects
# must name one version and one build, and each build number must be above the newest one its
# store holds (docs/RELEASING.md, Versions). The floors are those newest builds, as TestFlight and
# Play's App bundle explorer showed them for the last bump, and they move only with the numbers:
# each bump sets every floor to the newest build its store holds and every build above it, in one
# change. A floor raised alone, to the build this checkout names, fails this case; a floor left
# behind after an upload passes a build its store already holds.
floor_setup=12      # Setup, on both stores
floor_pos_ios=4     # POS, App Store Connect
floor_pos_android=3 # POS, Play
yml_setting() { # name: its values in ios-pos/project.yml, read as the POS iOS script reads them
  sed -nE 's/^[[:space:]]+'"$1"':[[:space:]]*"?([^"[:space:]]+)"?[[:space:]]*$/\1/p' "$repo/ios-pos/project.yml"
}
pbx_agrees() { # pbxproj name value: every configuration sets name to value, as the POS iOS script checks
  local lines
  lines=$(sed -nE 's/^[[:space:]]+('"$2"' = .*)$/\1/p' "$1")
  [[ -n $lines && -z $(grep -vxF "$2 = $3;" <<<"$lines" || true) ]]
}
gradle_version() { # gradle file: "versionName versionCode", read as scripts/release-android.sh reads them
  sed -nE 's/.*versionName[ =]+"([^"]+)".*/\1/p; s/.*versionCode[ =]+([0-9]+).*/\1/p' "$1" |
    paste -sd' ' - | awk '{print $2, $1}'
}
package_field() { # name: its values in apps/provisioner/package.json, read as the Setup iOS script reads them
  sed -nE 's/^[[:space:]]*"'"$1"'":[[:space:]]*"?([^",[:space:]]+)"?,?[[:space:]]*$/\1/p' \
    "$repo/apps/provisioner/package.json"
}
once_each() { # values...: each is one line, and not empty
  local value
  for value in "$@"; do [[ -n $value && $value != *$'\n'* ]] || return 1; done
}
above() { [[ $1 =~ ^[0-9]+$ ]] && ((10#$1 > $2)); } # number floor
cases=$((cases + 1))
echo "# versions: each app's project files name one version and build, and each build is above its floor"
pos_version=$(yml_setting MARKETING_VERSION) pos_build=$(yml_setting CURRENT_PROJECT_VERSION)
pos_pbx=$repo/ios-pos/FedimintCardPOS.xcodeproj/project.pbxproj
read -r pos_android_version pos_android_code <<<"$(gradle_version "$repo/android-relay/app/build.gradle.kts")"
expect "POS iOS: project.yml sets one version ($pos_version) and one build ($pos_build)" \
  once_each "$pos_version" "$pos_build"
expect "POS iOS: every configuration in the Xcode project has that version" \
  pbx_agrees "$pos_pbx" MARKETING_VERSION "$pos_version"
expect "POS iOS: every configuration in the Xcode project has that build" \
  pbx_agrees "$pos_pbx" CURRENT_PROJECT_VERSION "$pos_build"
expect "POS iOS: build $pos_build is above the floor ($floor_pos_ios)" above "$pos_build" "$floor_pos_ios"
expect "POS Android: versionCode $pos_android_code is above the floor ($floor_pos_android)" \
  above "$pos_android_code" "$floor_pos_android"
expect "POS: Android's versionName ($pos_android_version) is the iOS version" \
  is "$pos_android_version" "$pos_version"
pos_plist=$repo/ios-pos/FedimintCardPOS/Info.plist
# shellcheck disable=SC2016 # the Info.plist names the build settings, not their values
expect "POS iOS: Info.plist takes its version and build from the build settings" \
  is "$(plist_value CFBundleShortVersionString "$pos_plist") $(plist_value CFBundleVersion "$pos_plist")" \
  '$(MARKETING_VERSION) $(CURRENT_PROJECT_VERSION)'
provisioner=$repo/apps/provisioner
provisioner_pbx=$provisioner/ios/FedimintProvisioner.xcodeproj/project.pbxproj
provisioner_version=$(package_field version) provisioner_build=$(package_field provisionerBuild)
expect "Setup: package.json sets one version ($provisioner_version) and one build ($provisioner_build)" \
  once_each "$provisioner_version" "$provisioner_build"
expect "Setup: Gradle has that version and build" \
  is "$(gradle_version "$provisioner/android/app/build.gradle")" "$provisioner_version $provisioner_build"
expect "Setup: every configuration in the Xcode project has that version" \
  pbx_agrees "$provisioner_pbx" MARKETING_VERSION "$provisioner_version"
expect "Setup: every configuration in the Xcode project has that build" \
  pbx_agrees "$provisioner_pbx" CURRENT_PROJECT_VERSION "$provisioner_build"
expect "Setup: Info.plist states that version" \
  is "$(plist_value CFBundleShortVersionString "$provisioner/ios/FedimintProvisioner/Info.plist")" "$provisioner_version"
# shellcheck disable=SC2016 # the Info.plist names the build setting, not its value
expect "Setup: Info.plist takes its build from the build settings" \
  is "$(plist_value CFBundleVersion "$provisioner/ios/FedimintProvisioner/Info.plist")" '$(CURRENT_PROJECT_VERSION)'
expect "Setup: build $provisioner_build is above the floor ($floor_setup)" \
  above "$provisioner_build" "$floor_setup"

echo
if ((failed)); then
  echo "$failed failed, in $cases cases" >&2
  exit 1
fi
echo "all passed, $cases cases"
