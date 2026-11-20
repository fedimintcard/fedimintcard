# Releasing the POS and Setup apps

Store builds of Fedimint Card POS and Fedimint Card Setup are made by hand on the Mac that holds the
signing keys, from `main` at the commit being released. The POS scripts build the offline core
(`host/pos-core`) from that checkout into their app. The artifacts land in `dist/`, which git
ignores. Tags and store submissions are made by hand.

## Before any build

Release builds run in a worktree of their own at the release commit, never in a checkout used for
anything else: the POS scripts run cargo in the checkout's `host/target` (or in `CARGO_TARGET_DIR`
when it is set), and every script builds whatever the tree holds.

```sh
git fetch origin
git worktree add --detach ../fedimintcard-release origin/main
cd ../fedimintcard-release
git status --short     # prints nothing
git log -1 --oneline   # the commit to tag
```

For a later release, fetch again and run `git switch --detach origin/main` in that worktree. A new
worktree has none of the gitignored files another checkout holds, such as `local.properties` in the
two Android projects: copy them in, or set `ANDROID_HOME`.

Only the Setup iOS script refuses a tree with changes or untracked files itself. The POS scripts
and the Android script do not check, so `git status --short` must print nothing before them.

## Versions

Bump the versions on `main` before building. The bump is the last change merged before the builds,
and nothing merges between it and them, so every build comes from the bump's commit.

- POS, iOS: `MARKETING_VERSION` and `CURRENT_PROJECT_VERSION` in `ios-pos/project.yml`, then
  `xcodegen generate` in `ios-pos/` and commit the regenerated
  `FedimintCardPOS.xcodeproj/project.pbxproj`. The release script stops when the two disagree.
- POS, Android: `versionName` and `versionCode` in `android-relay/app/build.gradle.kts`.
- Setup, both platforms: `version` and `provisionerBuild` in `apps/provisioner/package.json`, then
  write them into the Android and Xcode projects and check them:

  ```sh
  cd apps/provisioner && node scripts/sync-version.cjs && node scripts/sync-version.cjs --check
  ```

Each store refuses a build number it already has, so each new number must be above the newest one
the store holds: look in TestFlight (App Store Connect) and in Play Console's App bundle explorer
before the bump merges.

## iOS: POS app, TestFlight

```sh
ios-pos/scripts/release-ios.sh
```

| Environment | What it holds |
|---|---|
| `ASC_KEY_PATH` | the App Store Connect API key file (`.p8`) |
| `ASC_KEY_ID` | that key's ID |
| `ASC_ISSUER_ID` | the issuer ID of the key's team |

One run archives and uploads: running the script is the TestFlight upload, and it has no
archive-only mode. It refuses to start without the three `ASC_` variables or the key file. It builds
the core from the checkout on every run, because a core an earlier build left in `ios-pos/Packages`
is gitignored and can come from another commit. It archives with automatic signing, then exports
with `ios-pos/ExportOptions.plist` and uploads the build to App Store Connect itself. The API key
authenticates both steps: with it, automatic signing fetches the archive's development profile, so
no Apple ID has to be signed in to Xcode. The archive is signed with the team's Apple Development
certificate from the login keychain. The script needs Xcode and the Rust targets
`aarch64-apple-ios`, `aarch64-apple-ios-sim` and `x86_64-apple-ios`.

Artifacts:

- `dist/ios/FedimintCardPOS-<version>-<build>.xcarchive`, the archive; its path is the script's
  last line.
- `dist/ios/export-<version>-<build>/`, the export's summary and logs.

## iOS: Setup app, TestFlight

Two runs, with the same three `ASC_` variables as the POS script. The first archives and checks the
archive, and never uploads:

```sh
apps/provisioner/scripts/release-ios.sh build
```

It refuses to start without the `ASC_` variables or the key file. Then it stops at the first of
these that fails, before anything slow runs:

1. `git status` shows nothing, untracked files included. It prints the commit.
2. `SKIP_BUNDLING` is unset; with it set, the archive would carry no JS bundle.
3. `dist/ios/FedimintProvisioner-<version>-<build>.xcarchive` and its `.commit` do not exist yet,
   with the version and build from `apps/provisioner/package.json`, so a rebuild never replaces an
   archive that records an upload.
4. Node is 22 or newer.
5. The release gates the Android build runs pass: `node scripts/sync-version.cjs --check` and
   `node scripts/configure-service.cjs --check --production`.
6. `tools/cardkit/gp/load-file.ts` names the load file's sha256.

Then it removes the installed copy of cardkit, runs `yarn install --frozen-lockfile --check-files`
(yarn copies `tools/cardkit` at install time, and a stale copy ships the old applet) and checks the
new copy matches `tools/cardkit` file for file. It runs `yarn test` and `pod install`, then stops if
`git status` shows any change (a file the preparation rewrote belongs in a commit on `main` first)
or HEAD is no longer the commit it printed. It archives the workspace with automatic signing and
checks the archive: the bundle id is `org.fedimintcard.provisioner`, the version and build are
`package.json`'s, and the app's `main.jsbundle` holds every registry origin the configuration gate
printed and the load file hash. The archive holds whatever the tree held while xcodebuild ran, so
it checks `git status` and HEAD once more: a change or a commit in the worktree during the archive
stops it, with no `.commit` written. Then it writes the commit to the `.commit` file and prints a
summary: bundle id, version and build, commit and team. Its last line is the archive's path.

Read the summary. The upload is the release itself, so it waits for the decision to release; then
upload that archive in a second run:

```sh
apps/provisioner/scripts/release-ios.sh upload dist/ios/FedimintProvisioner-<version>-<build>.xcarchive
```

It refuses an archive without the `.commit` a build writes, an archive of another app, and an
archive that records an upload of its build. It prints the bundle id, version, build and commit it
will upload, then exports with `apps/provisioner/ios/ExportOptions.plist` and uploads the build to
App Store Connect. The script needs Xcode, CocoaPods, yarn and Node 22 or newer.

Artifacts:

- `dist/ios/FedimintProvisioner-<version>-<build>.xcarchive`, the archive; its path is the last
  line of `build`.
- `dist/ios/FedimintProvisioner-<version>-<build>.commit`, the commit it was built from.
- `dist/ios/export-FedimintProvisioner-<version>-<build>/`, the export's summary and logs; its path
  is the last line of `upload`.

App Store Connect processes an upload before TestFlight offers it. Each app's `Info.plist` answers
export compliance. What to Test and the external testing groups are set in App Store Connect; the
first build of a version added to an external group goes to Beta App Review.

## Android: POS app, Play

```sh
scripts/release-android.sh pos
```

| Environment | What it holds |
|---|---|
| `ANDROID_NDK_HOME` | the NDK the core builds with; unset, the newest under `~/Library/Android/sdk/ndk` |
| `FEDIMINTCARD_ANDROID_UPLOAD_KEYSTORE` | the upload keystore; the script has a default |
| `FEDIMINTCARD_ANDROID_UPLOAD_KEY_ALIAS` | the upload key's alias; the script has a default |

The upload key's password lives in the macOS Keychain, service `fedimintcard-android-upload`. The
script reads it into Gradle's environment and never prints it. With no NDK the script stops before
it reads the Keychain or starts Gradle. It needs JDK 17, the Android SDK, `cargo-ndk` and the Rust
targets `aarch64-linux-android` and `x86_64-linux-android`. With no argument it also builds the
Setup app's bundle.

The artifact is `dist/android/pos-<versionName>-<versionCode>.aab`; its path is the script's last
line. The script checks the bundle holds `libfedimint_card_pos_core.so` under `base/lib/arm64-v8a`
and `base/lib/x86_64`, and stops before copying a bundle without them.

Upload the bundle by hand in Play Console, to the track the release is for, with its release notes.
Play re-signs it with the app-signing key, and its review follows.

## Android: Setup app, Play

The Android script neither checks the tree nor refreshes the app's copy of cardkit, so prepare the
app first:

```sh
cd apps/provisioner
rm -rf node_modules/@fedimintcard/cardkit && yarn install --frozen-lockfile --check-files && yarn test
(cd android && ./gradlew -q generateCodegenArtifactsFromSchema && rm -rf app/.cxx)
cd ../..
scripts/release-android.sh provisioner
```

The `yarn` line installs a fresh copy of cardkit and runs the unit tests, which fail while the copy
differs from `tools/cardkit`. The `gradlew` line, needed after any `yarn install`, regenerates the
codegen sources and drops the native build cache. The script signs with the same upload key as the
POS bundle, from the same Keychain item. Gradle refuses the release build when the upload key's
variables are missing, when `sync-version.cjs --check` fails, or when
`configure-service.cjs --check --production` fails.

The artifact is `dist/android/provisioner-<versionName>-<versionCode>.aab`; its path is the
script's last line. Upload it by hand in Play Console, as for the POS app.

## Order of a release

1. Merge what the release carries, then the version bump last.
2. Merge nothing between the bump and the builds.
3. Build from the bump's commit, in a worktree of its own (Before any build): the POS iOS script,
   which uploads; the Setup iOS `build`, its summary read, then `upload`; the two Android bundles.
4. Upload the Android bundles by hand, and tag the commit by hand.

Where the bump changes an app's version, the first iOS build of that version goes to Beta App Review
(above) before external testers get it.

## Tests

`scripts/test-release-scripts.sh` runs the three scripts (`scripts/release-android.sh`,
`ios-pos/scripts/release-ios.sh` and `apps/provisioner/scripts/release-ios.sh`) against stand-ins
for Gradle, xcodebuild, the core build, the Keychain, node, yarn and CocoaPods, so it builds, signs
and uploads nothing. The Setup cases run in a scratch git checkout. CI runs the suite on macOS,
where the scripts get `/bin/bash` 3.2 and the BSD tools as on the release Mac, and on Linux after
`shellcheck`.
