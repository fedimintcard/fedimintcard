#!/usr/bin/env bash
# Archive Fedimint Card POS for iOS and upload it to App Store Connect (TestFlight), in one run:
#
#   1. ios-pos/scripts/build-poscore.sh builds the core: the library and its bindings come from one
#      build (UniFFI stops the app when they differ). SKIP_POSCORE=1 skips it when it already ran
#      on this commit.
#   2. xcodebuild archives dist/ios/FedimintCardPOS-<version>-<build>.xcarchive. The version and
#      build are MARKETING_VERSION and CURRENT_PROJECT_VERSION in ios-pos/project.yml; the Xcode
#      project must carry the same values (xcodegen generate), or the script stops first.
#   3. xcodebuild -exportArchive signs the archive for the App Store with ios-pos/ExportOptions.plist
#      and uploads it, authenticated by an App Store Connect API key. Its summary and logs go to
#      dist/ios/export-<version>-<build>.
#
# Needs ASC_KEY_PATH (the API key's .p8 file), ASC_KEY_ID and ASC_ISSUER_ID; without all three, or
# without the key file, it stops before building anything. Its own messages name the variables
# without their values. Runs from the repository root wherever it is called from. The last line
# printed is the archive's path. The whole release: docs/RELEASING.md.
set -euo pipefail

missing=()
for var in ASC_KEY_PATH ASC_KEY_ID ASC_ISSUER_ID; do
	[[ -n ${!var:-} ]] || missing+=("$var")
done
if ((${#missing[@]})); then
	echo "set ${missing[*]} (an App Store Connect API key) to archive and upload" >&2
	exit 1
fi
# A relative key path is the caller's; the script works from the repository root.
case $ASC_KEY_PATH in /*) ;; *) ASC_KEY_PATH=$PWD/$ASC_KEY_PATH ;; esac
[[ -f $ASC_KEY_PATH && -r $ASC_KEY_PATH ]] || { echo "ASC_KEY_PATH names no readable key file" >&2; exit 1; }

cd "$(dirname "$0")/../.."
root=$PWD

setting() { # name: its one value in ios-pos/project.yml
	local found
	found=$(sed -nE 's/^[[:space:]]+'"$1"':[[:space:]]*"?([^"[:space:]]+)"?[[:space:]]*$/\1/p' ios-pos/project.yml)
	if [[ -z $found || $found == *$'\n'* ]]; then
		echo "ios-pos/project.yml must set $1 once" >&2
		return 1
	fi
	echo "$found"
}
version=$(setting MARKETING_VERSION)
build=$(setting CURRENT_PROJECT_VERSION)

# xcodebuild reads the generated project, so every configuration there must agree with project.yml.
pbxproj=ios-pos/FedimintCardPOS.xcodeproj/project.pbxproj
for pair in "MARKETING_VERSION=$version" "CURRENT_PROJECT_VERSION=$build"; do
	name=${pair%%=*} value=${pair#*=}
	lines=$(sed -nE 's/^[[:space:]]+('"$name"' = .*)$/\1/p' "$pbxproj")
	others=$(grep -vxF "$name = $value;" <<<"$lines" || true)
	if [[ -z $lines || -n $others ]]; then
		echo "the Xcode project's $name differs from ios-pos/project.yml ($value): run xcodegen generate in ios-pos and commit" >&2
		exit 1
	fi
done

if [[ ${SKIP_POSCORE:-} != 1 ]]; then
	ios-pos/scripts/build-poscore.sh
fi

archive=dist/ios/FedimintCardPOS-$version-$build.xcarchive
mkdir -p dist/ios
xcodebuild -project ios-pos/FedimintCardPOS.xcodeproj -scheme FedimintCardPOS -configuration Release \
	-destination generic/platform=iOS CODE_SIGN_STYLE=Automatic DEVELOPMENT_TEAM=H7UAM79QQP \
	-allowProvisioningUpdates archive -archivePath "$archive"
xcodebuild -exportArchive -archivePath "$archive" -exportOptionsPlist ios-pos/ExportOptions.plist \
	-exportPath "dist/ios/export-$version-$build" -allowProvisioningUpdates \
	-authenticationKeyPath "$ASC_KEY_PATH" -authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID"
echo "$root/$archive"
