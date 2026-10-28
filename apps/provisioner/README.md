# Fedimint Provisioner

The phone-side operator app for programming blank J3R180 JavaCards into Fedimint
cards, one tap at a time, in batches. It installs the Fedimint card applet over
NFC, lets the card generate its keys, sets the card's PIN, and loads value onto
it from the operator's wallet.

Read `docs/PROVISIONER-PLAN.md` § 5 for what the app is and § 8.6 for the
identity values below. The engine (`tools/cardkit`) is consumed as an installed
package; `src/cardkit.ts` is the only module that imports it.

```text
SignIn → BatchSetup → Loop (one card per tap, with stops) → BatchSummary → Settings
```

That is the whole surface. There is no card browser, no load-test bench and no
reset flow: those live in `host/sidecar` (`cardctl`) on the laptop.

## Running it

Prerequisites: Node ≥ 22, JDK 17 and the Android SDK for Android, Xcode (and
CocoaPods) for iOS. The engine's dependency is a `file:` package, so it must be
installed, not linked in place:

```sh
cd apps/provisioner
yarn install                 # resolves @fedimintcard/cardkit from ../../tools/cardkit
(cd ios && pod install)      # iOS only, after yarn install
yarn start                   # Metro
yarn android                 # or: yarn ios
```

Tests and lint:

```sh
yarn test
yarn lint
```

Note that `yarn install` here and the engine's own `yarn install` in
`tools/cardkit` are separate: cardkit checks its dependency versions in its own
lockfile, the app bundles the source through Metro.

## Identity

Decided 2026-10-28 (plan § 8.6). Do not change one of these without the others:
the store builds, the NFC entitlement and the JS component name all assume this
set.

| Field | Value |
|---|---|
| Android `applicationId` | `org.fedimintcard.provisioner` |
| Android `namespace` | `org.fedimintcard.provisioner` |
| Android package (Kotlin sources) | `org.fedimintcard.provisioner` |
| Android `app_name` | `FedimintProvisioner` |
| iOS `PRODUCT_BUNDLE_IDENTIFIER` | `org.fedimintcard.provisioner` |
| iOS project / scheme / target | `FedimintProvisioner` |
| iOS `CFBundleDisplayName` | `FedimintProvisioner` |
| RN `name` / `displayName` (`app.json`) | `FedimintProvisioner` |

`org.fedimintcard` matches `android-relay` (`org.fedimintcard.relay`), so the two
apps install side by side and neither can capture the other's intents. The
display name is deliberately identical on both platforms.

The build-time constants the app shows (AIDs, load-file sha256, cardkit version,
identity) are frozen in `src/settings.tsx`; runtime settings (federation,
registry endpoint, top-up amounts, theme) are editable in Settings and persisted
under `prov.settings.v1`.

## Layout

```text
App.tsx              the five routes
index.js             AppRegistry entry; installs crypto.getRandomValues
src/cardkit.ts       the only import of @fedimintcard/cardkit, plus FEDIMINT_PLAN
src/settings.tsx     build-time constants (frozen) and runtime settings (persisted)
src/store.tsx        the batch state machine: phases, stops, PIN policy, loop index
src/pin.ts           PIN entry, generation and the wipe-on-leave rules
src/api.ts           RegistryClient, LocalClient (sidecar), RegistryOutbox
src/session.ts       withCard: the ISO-DEP transceiver over react-native-nfc-manager
src/demo/            the fake card and its scripted stops (store review)
src/screens/         SignIn, BatchSetup, Loop, BatchSummary, Settings
android/, ios/       native shells only; nothing card-specific lives here
```

`metro.config.js` resolves the engine through its package entry points with
package exports on — no `watchFolders` alias, so there is exactly one copy of
the engine in the bundle.

## Status

Experimental. The app programs a card and loads value onto it; the card refuses
to sign without the PIN set here, and it has no spending limits, no owner key
and no note list. The app makes no claim to custody.

Ported from the Cashu provisioner (`cashucard/cashucard`, MIT), with the trimmed
screen surface and this card's install plan.
