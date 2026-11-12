# Releasing the POS apps

Store builds of Fedimint Card POS are made by hand on the Mac that holds the signing keys, from a
clean checkout of `main` at the commit being released. Each script builds the offline core
(`host/pos-core`) from that checkout into its app. The artifacts land in `dist/`, which git
ignores.

## Versions

Bump the versions on `main` before building:

- iOS: `MARKETING_VERSION` and `CURRENT_PROJECT_VERSION` in `ios-pos/project.yml`, then
  `xcodegen generate` in `ios-pos/` and commit the regenerated
  `FedimintCardPOS.xcodeproj/project.pbxproj`. The release script stops when the two disagree.
- Android: `versionName` and `versionCode` in `android-relay/app/build.gradle.kts`.

Each store refuses a build number it already has.

## iOS: TestFlight

```sh
ios-pos/scripts/release-ios.sh
```

| Environment | What it holds |
|---|---|
| `ASC_KEY_PATH` | the App Store Connect API key file (`.p8`) |
| `ASC_KEY_ID` | that key's ID |
| `ASC_ISSUER_ID` | the issuer ID of the key's team |

The script refuses to start without the three `ASC_` variables or the key file. It builds the core
from the checkout on every run, because a core an earlier build left in `ios-pos/Packages` is
gitignored and can come from another commit. It archives with automatic signing, then exports with
`ios-pos/ExportOptions.plist` and uploads the build to App Store Connect itself. The API key
authenticates both steps: with it, automatic signing fetches the archive's development profile, so
no Apple ID has to be signed in to Xcode. The archive is signed with the team's Apple Development
certificate from the login keychain. The script needs Xcode and the Rust targets
`aarch64-apple-ios`, `aarch64-apple-ios-sim` and `x86_64-apple-ios`.

Artifacts:

- `dist/ios/FedimintCardPOS-<version>-<build>.xcarchive`, the archive; its path is the script's
  last line.
- `dist/ios/export-<version>-<build>/`, the export's summary and logs.

App Store Connect processes the upload before TestFlight offers it. The app's `Info.plist` answers
export compliance. What to Test and the external testing groups are set in App Store Connect; the
first build of a version added to an external group goes to Beta App Review.

## Android: Play

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

## Tests

`scripts/test-release-scripts.sh` runs both scripts against stand-ins for Gradle, xcodebuild, the
core build and the Keychain, so it builds, signs and uploads nothing. CI runs it on macOS, where the
scripts get `/bin/bash` 3.2 and the BSD tools as on the release Mac, and on Linux after
`shellcheck`.
