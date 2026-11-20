#!/usr/bin/env bash
# Archive Fedimint Card Setup (apps/provisioner) for iOS, and upload an archive to App Store Connect
# (TestFlight), as two separate runs:
#
#   apps/provisioner/scripts/release-ios.sh build             # archive and check it; never uploads
#   apps/provisioner/scripts/release-ios.sh upload <archive>  # upload an archive build made
#
# build stops at the first guard that fails, each before anything slow runs:
#   1. the App Store Connect API key (below) is set and its file is readable;
#   2. the checkout is a git work tree where git status shows nothing, untracked files included;
#      the commit is printed, and recorded beside the archive once the archive passes its checks;
#   3. SKIP_BUNDLING is unset: React Native's build phase leaves the JS bundle out when it is set;
#   4. the version and build are "version" and "provisionerBuild" in apps/provisioner/package.json,
#      and dist/ios/FedimintProvisioner-<version>-<build>.xcarchive and its .commit do not exist yet,
#      so a rebuild never replaces an archive that records an upload;
#   5. node is 22 or newer (package.json engines);
#   6. the release gates Gradle runs for Android pass: scripts/sync-version.cjs --check, then
#      scripts/configure-service.cjs --check --production, whose registry origins the bundle must
#      hold;
#   7. tools/cardkit/gp/load-file.ts names the load file's sha256, which the bundle must hold too.
# Then it removes the installed copy of cardkit and runs yarn install --frozen-lockfile --check-files,
# which copies tools/cardkit again: yarn copies a file: dependency at install time, and a stale copy
# ships the old applet with every test green. The copy must match tools/cardkit file for file, as
# src/cardkitCopy.test.ts checks. Then yarn test, pod install, and git status must still show
# nothing. xcodebuild archives the workspace with automatic signing, and the archive must be this
# app at package.json's version and build, with a main.jsbundle that holds every registry origin and
# the load file hash (its first 16 hex digits) and no LOCAL SESSION. The commit goes to the archive's
# .commit file, a summary follows, and the last line printed is the archive's path.
#
# upload takes an archive build made (the .commit beside it says so) of this app that records no
# upload of its build, prints what it will upload and from which commit, then runs xcodebuild
# -exportArchive with apps/provisioner/ios/ExportOptions.plist, which signs the archive for the App
# Store and uploads it. The last line printed is the export's directory,
# dist/ios/export-FedimintProvisioner-<version>-<build>, with its summary and logs.
#
# Both runs authenticate xcodebuild with an App Store Connect API key: ASC_KEY_PATH (the key's .p8
# file), ASC_KEY_ID and ASC_ISSUER_ID. The script never opens the key file; its path and the two IDs
# reach xcodebuild as arguments, and no key bytes or password is ever one. Its own messages name the
# variables without their values. Runs from the repository root wherever it is called from. The
# whole release: docs/RELEASING.md.
set -euo pipefail

usage() {
	echo "usage: apps/provisioner/scripts/release-ios.sh build | upload <archive>" >&2
	exit 2
}
case ${1:-} in
build) if (($# != 1)); then usage; fi ;;
upload) if (($# != 2)) || [[ -z $2 ]]; then usage; fi ;;
*) usage ;;
esac

missing=()
for var in ASC_KEY_PATH ASC_KEY_ID ASC_ISSUER_ID; do
	[[ -n ${!var:-} ]] || missing+=("$var")
done
if ((${#missing[@]})); then
	echo "set ${missing[*]} (an App Store Connect API key) to archive and upload" >&2
	exit 1
fi
# A relative path is the caller's; the script works from the repository root.
case $ASC_KEY_PATH in /*) ;; *) ASC_KEY_PATH=$PWD/$ASC_KEY_PATH ;; esac
[[ -f $ASC_KEY_PATH && -r $ASC_KEY_PATH ]] || { echo "ASC_KEY_PATH names no readable key file" >&2; exit 1; }
auth=(-authenticationKeyPath "$ASC_KEY_PATH" -authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID")
if [[ $1 == upload ]]; then
	archive=${2%/}
	case $archive in /*) ;; *) archive=$PWD/$archive ;; esac
fi

cd "$(dirname "$0")/../../.."
root=$PWD
app_id=org.fedimintcard.provisioner

plist_values() { # key plist: each value that follows <key>key</key>, in Xcode's XML layout
	sed -nE '/^[[:space:]]*<key>'"$1"'<\/key>[[:space:]]*$/{n;s/^[[:space:]]*<(string|integer)>([^<]*)<\/(string|integer)>[[:space:]]*$/\2/p;}' "$2"
}

if [[ $1 == upload ]]; then
	commit_file=${archive%.xcarchive}.commit
	if [[ $archive != *.xcarchive || ! -f $archive/Info.plist || ! -f $commit_file ]]; then
		echo "$archive was not made by release-ios.sh build" >&2
		exit 1
	fi
	id=$(plist_values CFBundleIdentifier "$archive/Info.plist")
	if [[ $id != "$app_id" ]]; then
		echo "the archive is for ${id:-no bundle id}, not $app_id" >&2
		exit 1
	fi
	version=$(plist_values CFBundleShortVersionString "$archive/Info.plist")
	build=$(plist_values CFBundleVersion "$archive/Info.plist")
	# Xcode records an upload in the archive; a second upload of a build is one the store refuses.
	while read -r uploaded; do
		if [[ $uploaded == "$build" ]]; then
			echo "$archive already records an upload of build $build" >&2
			exit 1
		fi
	done < <(plist_values uploadedBuildNumber "$archive/Info.plist")
	commit=$(cat "$commit_file")
	echo "will upload $id $version ($build), built from $commit, to App Store Connect (TestFlight)"
	export_dir=dist/ios/export-FedimintProvisioner-$version-$build
	mkdir -p dist/ios
	xcodebuild -exportArchive -archivePath "$archive" -exportOptionsPlist apps/provisioner/ios/ExportOptions.plist \
		-exportPath "$export_dir" -allowProvisioningUpdates "${auth[@]}"
	echo "$root/$export_dir"
	exit 0
fi

clean() { # message: stops the script unless git status shows nothing, untracked files included
	local changes
	changes=$(git status --porcelain --untracked-files=normal)
	if [[ -n $changes ]]; then
		echo "$1" >&2
		echo "$changes" >&2
		exit 1
	fi
}
if [[ $(git rev-parse --is-inside-work-tree 2>/dev/null) != true ]]; then
	echo "$root is not a git checkout: build from a clean checkout of the commit to release" >&2
	exit 1
fi
clean "the working tree is not clean (git status --short): commit, stash or remove these first"
commit=$(git rev-parse HEAD)
echo "commit $commit"

if [[ -n ${SKIP_BUNDLING:-} ]]; then
	echo "SKIP_BUNDLING is set: the archive would carry no JS bundle" >&2
	exit 1
fi

app=apps/provisioner
field() { # name: its one value in apps/provisioner/package.json
	local found
	found=$(sed -nE 's/^[[:space:]]*"'"$1"'":[[:space:]]*"?([^",[:space:]]+)"?,?[[:space:]]*$/\1/p' "$app/package.json")
	if [[ -z $found || $found == *$'\n'* ]]; then
		echo "$app/package.json must set $1 once" >&2
		return 1
	fi
	echo "$found"
}
version=$(field version)
build=$(field provisionerBuild)
archive=dist/ios/FedimintProvisioner-$version-$build.xcarchive
commit_file=${archive%.xcarchive}.commit
for path in "$archive" "$commit_file"; do
	if [[ -e $path ]]; then
		echo "$root/$path exists: move it aside or bump provisionerBuild" >&2
		exit 1
	fi
done

node_version=$(node --version 2>/dev/null || true)
major='^v([0-9]+)\.'
if [[ ! $node_version =~ $major ]] || ((10#${BASH_REMATCH[1]} < 22)); then
	echo "node 22 or newer is required ($app/package.json engines); found ${node_version:-none}" >&2
	exit 1
fi

if ! (cd "$app" && node scripts/sync-version.cjs --check); then
	echo "release gate failed: node scripts/sync-version.cjs --check" >&2
	exit 1
fi
if ! gate=$(cd "$app" && node scripts/configure-service.cjs --check --production); then
	if [[ -n $gate ]]; then echo "$gate"; fi
	echo "release gate failed: node scripts/configure-service.cjs --check --production" >&2
	exit 1
fi
echo "$gate"
# The gate prints "  registry <name>: <origin>" for each origin the app is built with.
origins=$(sed -nE 's/^[[:space:]]+registry [^:[:space:]]+: (https:\/\/[^[:space:]]+)$/\1/p' <<<"$gate")
if [[ -z $origins ]]; then
	echo "configure-service.cjs printed no registry origin: its output format changed" >&2
	exit 1
fi

load_file=tools/cardkit/gp/load-file.ts
load_hash=$(sed -nE "s/^[[:space:]]*sha256:[[:space:]]*'([0-9a-f]{16})[0-9a-f]{48}',?[[:space:]]*\$/\\1/p" "$load_file")
if [[ -z $load_hash || $load_hash == *$'\n'* ]]; then
	echo "no load file hash in $load_file" >&2
	exit 1
fi

copy=$app/node_modules/@fedimintcard/cardkit
rm -rf "$copy"
(cd "$app" && yarn install --frozen-lockfile --check-files)
cardkit_files() { # dir: index.ts and gp/*.ts in it, the files Metro bundles
	local file
	if [[ -f $1/index.ts ]]; then echo index.ts; fi
	for file in "$1"/gp/*.ts; do
		if [[ -f $file ]]; then echo "gp/${file##*/}"; fi
	done
}
while read -r file; do
	[[ -n $file ]] || continue
	if [[ ! -f tools/cardkit/$file || ! -f $copy/$file ]] || ! cmp -s "tools/cardkit/$file" "$copy/$file"; then
		echo "the installed cardkit differs from tools/cardkit: $file" >&2
		exit 1
	fi
done < <({ cardkit_files tools/cardkit; cardkit_files "$copy"; } | sort -u)
if ! (cd "$app" && yarn test); then
	echo "yarn test failed" >&2
	exit 1
fi
(cd "$app/ios" && pod install)
clean "the tree changed while preparing (yarn or pod install rewrote a tracked file):"

mkdir -p dist/ios
xcodebuild -workspace "$app/ios/FedimintProvisioner.xcworkspace" -scheme FedimintProvisioner -configuration Release \
	-destination generic/platform=iOS -allowProvisioningUpdates "${auth[@]}" archive -archivePath "$archive"

info=$archive/Info.plist
if [[ ! -f $info ]]; then
	echo "xcodebuild wrote no archive at $root/$archive" >&2
	exit 1
fi
id=$(plist_values CFBundleIdentifier "$info")
if [[ $id != "$app_id" ]]; then
	echo "the archive is for ${id:-no bundle id}, not $app_id" >&2
	exit 1
fi
archived_version=$(plist_values CFBundleShortVersionString "$info")
archived_build=$(plist_values CFBundleVersion "$info")
if [[ $archived_version != "$version" || $archived_build != "$build" ]]; then
	echo "the archive says $archived_version ($archived_build), package.json says $version ($build)" >&2
	exit 1
fi
bundle=$archive/Products/Applications/FedimintProvisioner.app/main.jsbundle
if [[ ! -f $bundle ]]; then
	echo "the app has no main.jsbundle" >&2
	exit 1
fi
while read -r origin; do
	if ! grep -aqF -- "$origin" "$bundle"; then
		echo "main.jsbundle does not hold the registry origin $origin" >&2
		exit 1
	fi
done <<<"$origins"
if ! grep -aqF -- "$load_hash" "$bundle"; then
	echo "main.jsbundle does not hold the load file hash $load_hash" >&2
	exit 1
fi
if grep -aqF 'LOCAL SESSION' "$bundle"; then
	echo "main.jsbundle holds LOCAL SESSION" >&2
	exit 1
fi

echo "$commit" >"$commit_file"
team=$(plist_values Team "$info")
echo "bundle id: $id"
echo "version: $version ($build)"
echo "commit: $commit"
echo "team: ${team:-none recorded}"
echo "main.jsbundle holds ${origins//$'\n'/, }, the load file hash $load_hash and no LOCAL SESSION"
echo "next: apps/provisioner/scripts/release-ios.sh upload $root/$archive"
echo "$root/$archive"
