# Fedimint Provisioner

The phone-side operator app for programming blank J3R180 JavaCards into Fedimint
cards, one tap at a time, in batches. It installs the Fedimint card applet over
NFC, locks its card manager to the card's own keys from ops, lets the card
generate its keys, sets its offline limit, gets the registry's certificate over the
card's attestation key written onto it, and sets the card's PIN (applet 3.0, all in
the one install tap; docs/OFFLINE-PROTOCOL.md § 5). Value comes from the holder's wallet
(docs/OPS-PLAN.md § 6).

Read `docs/PROVISIONER-PLAN.md` § 5 for what the app is and § 8.6 for the
identity values below. The engine (`tools/cardkit`) is consumed as an installed
package; `src/cardkit.ts` is the only module that imports it.

```text
SignIn → BatchSetup → Loop (one card per tap, with stops) → BatchSummary → Settings
```

The install tap, after the card manager is locked to the card's own keys: GEN_KEY on
slots 0-7, then `SET_LIMIT` (the batch's offline limit per day, set in BatchSetup,
default 10,000 sat, 0 for none), `ATTEST_INIT` (the card makes a P-256 key),
`POST /registry/cards/:short_id/attestation` with the operator session (the registry
answers the certificate), `SET_CERT`, `SET_REGISTRY_KEY` (the registry's certificate
key from the public `GET /registry/cert-key`, fetched once per batch before the first
tap so the NFC session makes no network call for it; skipped when `GET_STATE` byte 58
is already 1, and on a card whose `GET_STATE` is 58 bytes), then `SET_PIN` and `VERIFY_PIN`. A tap lifted
anywhere in it is finished by the next tap of the same card: the offline steps are
idempotent before the PIN (`SET_LIMIT` again, `ATTEST_INIT` again until `SET_CERT`
took, `SET_REGISTRY_KEY` until byte 58 says it is set), and a card that already holds its certificate keeps its keys, because the
certificate is over slot 0's key and cannot be replaced. A card that answers SELECT
below 3.0 is finished the old way, without them. When the registry (not the card)
stops the finish, the screen shows the registry's own reason ("The registry refused the
card's certificate: …"), not the lifted-card text.

That is the whole surface, plus Settings → Reset a card (docs/OPS-PLAN.md O5).
There is no card browser and no load-test bench: those live in `host/sidecar`
(`cardctl`) on the laptop.

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

yarn copies the engine into `node_modules` at install time; it does not link
it. After any change under `tools/cardkit`, refresh the copy before building,
or the app installs whatever applet the old copy carries:

```sh
rm -rf node_modules/@fedimintcard/cardkit && yarn install --frozen-lockfile --check-files
```

`src/cardkitCopy.test.ts` fails while the copy differs from `tools/cardkit`, so
`yarn test` before a build catches it.

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
registry endpoint, top-up per card) are editable in Settings and persisted
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
to sign without the PIN set here and enforces the offline limit set here; it has no
owner key. The app makes no claim to custody.

Ported from the Cashu provisioner (`cashucard/cashucard`, MIT), with the trimmed
screen surface and this card's install plan.
