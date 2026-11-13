# Rollout: offline payments to production

Status: checklist, 2026-11-07, for `main` @ `3a0b0e8`; step 7 (the POS phones) added 2026-11-13.
Each step changes production and waits for the owner's OK. Production reached step 6 on
2026-11-07, with zion's till at `0.1.0:3` (built from `efdc3a6`). Since 2026-11-10 zion has run
`0.1.0:4` (built from `a630a22`, with the phones' kit and charge routes), and step 7 was drilled
on it on 2026-11-13 (`POS-OFFLINE.md` § Drill log). Staging ran steps 0 to 6 first (two e2e runs,
`OFFLINE-PLAN.md` § 10); its pass of step 7 is still to run.

What production had before step 1: registry v9 (Worker version `35521a72`, the claim fix), 2.1
cards, the zion till `0.1.0:1` or `0.1.0:2`, the Setup and POS apps from before applet 3.0.

What changes for cards already in the field: nothing. 2.1 cards keep working online with the new
till and registry; only cards set up after step 5 are 3.0 and can pay offline.

## 0. Before starting

- [ ] All suites green on the commit being rolled out: `mvn -B test`; `registry`: `npx vitest run`
      and `npx tsc --noEmit -p .`; `host`: `cargo test --workspace`; `apps/provisioner`: `npx jest`
      after `rm -rf node_modules/@fedimintcard/cardkit && yarn install --frozen-lockfile --check-files`;
      `tools/cardkit`: `npm test`; `python3 tools/cardkit/make-assets.py --check`.
- [ ] Note what is live, for rollback: `cd registry && npx wrangler deployments list | head`; zion's
      till version in StartOS; the app versions on the Pixel and Max.
- [ ] No charge open on the live registry for a card you will demo with (a balance tap on zion).

## 1. Certificate key (owner)

The registry signs card certificates and releases with it; production has none yet. Run once,
from `registry/` (a new key, never printed):

```text
! cd ~/Repos/fedimintcard/registry && openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -outform DER | base64 | tr -d '\n' | npx wrangler secret put CERT_SIGNING_KEY
```

- [ ] Check: after step 3, `curl -s https://registry.fedimintcard.org/registry/cert-key` answers
      `{"ok":true,"key":"04…"}`. Before step 3 the old Worker ignores the secret.
- The key never leaves Cloudflare. Losing it means new cards cannot be certified and no release
  can be signed until a new key is set; cards certified under the old key keep their
  certificates, but tills pin the new key, so set a new key only when you must.

## 2. Backup and restore drill

- [ ] Trigger or wait for the nightly backup; check `GET /registry/backup-status` (operator) is
      fresh.
- [ ] Fetch the newest object from the `fedimintcard-registry-backups` bucket and decrypt it:
      `node scripts/backup-drill.mjs fedimintcard-registry-backups <object key>` (identity at
      `~/.config/fedimintcard/backup-identity.txt`). Expect schema v9 and the live counts.

## 3. Registry v12

- [ ] `cd registry && npx wrangler deploy` (the top-level environment is production).
- [ ] Migrations v10–v12 run on the first request; they only add columns and tables
      (`attest_pub`, `attest_cert`, `offline_reports`, `pending_attestations`, `releases`).
- [ ] Check: `GET /registry/cert-key` (step 1); a live 2.1 card answers `GET /cards/<id>/slots`;
      a balance tap through zion on a 2.1 card works.
- Rollback: `npx wrangler rollback <previous version id>`. The old code reads a schema newer than
  it knows without touching the new tables, so a rollback is safe; certificates issued meanwhile
  stay in the table for a later redeploy.

## 4. zion till

- [ ] Build the StartOS package from this `main`: in `~/Repos/fedimintcard-till-startos`, move
      `upstream-project` to the commit, bump `startos/versions/current.ts` to the next version
      with release notes, `make`, commit, and push with the owner's OK. `0.1.0:3` (from
      `efdc3a6`) was the first with offline charges: 3.0 cards online, offline charges queued and
      sent later, spare slots, fee margin, releases. `0.1.0:4` (from `a630a22`) added the POS
      phones' kit and charge routes (step 7).
- [ ] Sideload `fedimintcard-till_x86_64.s9pk` on zion (owner; the Mac's CLI is not authorised).
- [ ] Check in StartOS: the new version, health green. Then `/v1/status` through the POS shows
      `registry_reachable: true`, `federation_reachable: true`, `offline.fee_margin: true`.
- [ ] The till fetches and keeps the registry's certificate key on its first online minute; offline
      charges need it, so let zion run online before the first village use.
- Rollback: sideload the previous `.s9pk`. An offline queue on zion must be empty first
  (`offline.queued: 0`), and from `0.1.0:4` on nothing may wait on any POS phone (step 7).

## 5. Apps

- [ ] Setup app, Android: release-signed with the upload key (keychain item
      `fedimintcard-android-upload`), from `apps/provisioner/android`: `./gradlew -q
      generateCodegenArtifactsFromSchema && rm -rf app/.cxx`, then `assembleRelease`. Check the
      bundle holds the 3.0 load file (`strings` of `assets/index.android.bundle` contains
      `3180fff668cd2f22`).
- [ ] Setup app, iOS: archive `FedimintProvisioner` for the Max.
- [ ] POS: `android-relay` and `ios-pos`, the same commit.
- [ ] Install on the Pixel and the Max. Setup app settings: **Live** registry, **Island Bitcoin
      Community**, daily limit **0** except for a village batch. POS: pair with zion again ("Pair a
      POS" on zion), since the Pixel's POS is paired with the staging till now.

## 6. First production card

- [ ] Set up one blank J3R180 on Live. Check on the USB reader: SELECT `03 00`, GET_STATE byte 56
      = 130 (certificate), 57 = 1, 58 = 1 (registry key).
- [ ] Top up by Lightning through zion, a balance tap (the spare reserve grows by 2 a tap), a charge.
- [ ] Optional, before a village: stop zion's internet for a charge or two and see "Paid offline",
      then restore it and watch `offline.queued` return to 0. This is zion's own offline path; the
      phones' was drilled on zion in step 7.

## 7. POS phones

A phone takes a 3.0 card on its own when it cannot reach the till, and syncs what it took once it
reaches the till again (`POS-OFFLINE.md`). Drilled on zion on 2026-11-13 (`POS-OFFLINE.md` § Drill
log).

- [ ] zion at `0.1.0:4` or later (step 4), run online for a minute first: the kit route
      (`GET /v1/offline/kit`) answers `503 offline_not_ready` until the till has the registry's
      certificate key, and a till before `0.1.0:4` has no kit route at all.
- [ ] Both POS apps built from one commit, `c0a0c2f` or later: an Android build from before it
      never opens its offline store (Settings: "Offline payments are off: Store: locking the
      offline store: try_lock() not supported"; PR #26).
- [ ] Pair each phone with zion ("Pair a POS" on zion) and open the app once while it reaches the
      till. Settings shows the kit: its age, its expiry about a week ahead, and the caps (200
      payments and 100,000 sats on zion), with 0 waiting to sync.
- [ ] One sale per phone in airplane mode ("Paid offline"), then airplane off with the app in
      front: within about a minute the phone shows 0 waiting to sync, and the till's balance
      has risen by what the sale paid.
- Order for a fleet: the till first (step 4), then one phone at a time, so the others keep
  selling. A phone gets another app build or pairing only when its Settings shows 0 waiting to
  sync and 0 waiting for their cards: what waits syncs only to the till it was taken for, and
  clearing the app's data, uninstalling or downgrading strands it. Each phone opens the app online
  at least once a week, the kit's life; while it reaches the till, the kit renews on its own.
  Before a day of offline sales, an online balance tap on each card writes the notes of its
  offline change and keys spare slots (at most 2 a tap); a card still takes only a few offline
  sales with change in a row (ENG-864).
- Rollback: only with 0 waiting on every phone; then the previous app on the phones, or the
  previous `.s9pk` on zion (step 4). A till before `0.1.0:4` takes no charges from phones.

## Afterwards

- Staging keeps running for tests: the laptop till on port 9878 with
  `~/.local/share/fedimint-card-staging`, from `a630a22` since 2026-11-11, listening on the LAN
  with caps of 3 payments and 100,000 msat (its wallet held 51,499,000 msat then). Both staging
  cards (`908901dc`, `4f5e6da3`) read 0 live msat on the staging registry on 2026-11-12: load one
  (`card load`) before step 7's staging pass.
- Open items: `OFFLINE-PLAN.md` § 10, and ENG-864 to ENG-867 (`POS-OFFLINE.md` § Drill log).
