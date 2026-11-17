#!/usr/bin/env bash
# Print the app's iOS deployment target: `deploymentTarget: iOS:` in ios-pos/project.yml, which
# xcodegen writes into the Xcode project. build-poscore.sh builds the core's libraries for it, as
# does the Mobile workflow's iOS job, so the app's build there finds them built.
set -euo pipefail

yml="$(cd "$(dirname "$0")/.." && pwd)/project.yml"
v=$(sed -nE 's/^[[:space:]]+iOS:[[:space:]]*"?([0-9][0-9.]*)"?[[:space:]]*$/\1/p' "$yml")
case "$v" in
	"" | *[!0-9.]*) echo "ios-pos/project.yml must set deploymentTarget iOS once" >&2; exit 1 ;;
esac
echo "$v"
