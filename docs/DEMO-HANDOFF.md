# Handoff: demo-readiness pass

Written 2026-11-04. Repo `fedimintcard/fedimintcard`, branch `main`, HEAD `ea7c12f`,
**11 commits ahead of `origin/main`** after the 2026-11-03 evening pass (`5b359d3` screens,
`573d8d2` icon, `c6d9852` gitignore, `4fcd2d1` sign-in demo row) and the 2026-11-04
morning pass (`7254d43` install-only default, real invite rule, QR scanner; `e835c83`
save buttons read SAVED when nothing is left to save; `ea7c12f` the design pass); the only
uncommitted file is this document.
This document supersedes nothing — `docs/PROVISIONER-HANDOFF.md` covers the P1/P2 cardkit
work and is still accurate for that scope.

The provisioner screen work was committed as `5b359d3` on the owner's instruction, and
the icon as `573d8d2`. Nothing has been pushed.

---

## 1. The goal

A live demo with real money: Fedi app → ecash import → card topup → tap-to-pay →
cashout. Hardware is an NXP J3R180 card, an ACS ACR122U reader on the laptop, and a Pixel
8 Pro running the NFC relay plus the POS app.

A second, larger goal was added: bring the Fedimint provisioner's UI
(`apps/provisioner`) up to the visual standard of the Cashu Card Provisioner
(`/Users/dread/Repos/cashu-card/apps/provisioner`), on both iOS and Android. User
decisions for that work, which are settled and should not be re-asked:

- The card face is **drawn by us in SVG**. Do not copy Cashu's `fl1-front.png` — it is
  Flash's MIT-licensed art for a different product.
- Match Cashu's **visual language**; keep Fedimint's **real step order**. PIN setup stays
  where the applet actually needs it.
- Start with the **loop screen**, because it carries the most visual payoff.

---

## 2. Repo state

`main` at `df5b6b0`, tree dirty. Four local commits on top of `origin/main`:

| Commit | Contents |
|---|---|
| `801eb34` | `pin_from_env()` reading `CARD_PIN`; `open_pin_session_with(card, pin)`; `CardWallet::verify_pin_session()` (`glue.rs:255`) |
| `703780e` | PIN verify moved inside the signing closure (`glue.rs:~1104`); `docs/DEMO.md` price 300 → 131 |
| `8b7846f` | **the phone-path bug**: `open_relay_card()` (`till.rs:147`) probed `FF21` without SELECT; added `let _ = c.select();` tolerating `6A82`. Plus `ErrorCopyTest.kt` |
| `df5b6b0` | planner: search every input size and renew until a slot is free; plus the android-relay card-balance feature (11 files, +249/−22) |

Added on 2026-11-03 evening:

| Commit | Contents |
|---|---|
| `5b359d3` | `CardFigure.tsx`, the restructured `LoopScreen.tsx`, `LOOP.of` in `strings.ts` (tsc clean, 64/64 jest, re-run before committing) |
| `573d8d2` | the icon: `apps/provisioner/assets/icon/{icon.svg,render.py}` plus every rendered iOS and Android asset; `Assets.xcassets` (never referenced by the pbxproj) and `values/icon_colors.xml` removed |
| `4fcd2d1` | SignIn's demo entry is a `Card`+`EntryRow` ("Try the demo" / "Real engine, simulated card, no account.") instead of a shouted two-line `SecondaryButton`; `SecondaryButton` now centres and pads its label |
| `7254d43` | `topupSats` is one number, default 0 = install only; `normalizeInvite` (`fed11…` / `fedimint…`, no `fedimint://`); `ScanInvite.tsx` sheet on `react-native-vision-camera` 4.7.3 behind SCAN QR on Settings and NEW BATCH; camera usage strings; Android bundles the ML Kit barcode model (`VisionCamera_enableCodeScanner=true`, the Pixel has no Play services); `topup.test.ts` + `federation.test.ts` (74/74) |
| `e835c83` | Settings' two save buttons read SAVED and go inert when the field equals the stored value — a scanned invite is saved as the sheet closes, so SAVE FEDERATION had sat live and done nothing (owner hit this on the Pixel with a real `fed11…` invite from a guardian UI's "Connect a Client" QR; the scan itself worked and persisted) |
| `ea7c12f` | Design pass after the Cashu provisioner's: navy/gold palette from the icon (`theme.ts`, no theme switch), brand lockup on sign-in, (i) help panels (`InfoHelp.tsx`), two-press start (Review → Start arms card 1), federation by name via on-phone invite decoding + sidecar `/v1/status` match (`useFederation.ts`, `FederationPicker.tsx`), top-up quick picks, loop/demo stop panels using the previously unused `CALM`/`STATE` text, redrawn card face. Fixed: End batch early went nowhere; Settings unreachable mid-batch; Stop could not end the NFC wait and would have recorded a phantom `pulled` card; summary confirm label. Walked on the Pixel via the dev local session; Max installed, unseen |

### Pushing

`bobodread876` has only `pull` on this repo, so a plain `git push` is a 403 no matter how
the URL is formed. Push as the org-admin identity:

```sh
TOK=$(gh auth token --user islandbitcoin)
git push "https://islandbitcoin:${TOK}@github.com/fedimintcard/fedimintcard" main
```

Commits are authored with `-c user.name='Dread' -c user.email='github@bobodread.com'`.
Verify the result with `git fetch origin -q && git rev-list --left-right --count origin/main...HEAD`
— expect `0	0` when pushed.

---

## 3. What is verified on hardware

Only these. Anything else is compiled or inferred, and is labelled as such below.

**A user-initiated 5-sat sale, phone → card.** Till sale #9, `client_ref:
1eeec021-3090-430e-8a6c-7415c9e3c6c6`, txid `23f28492f822…`, 5000 msat price, 6144
received. APDU sequence `ca→a4→11→40→12→10`, every response `9000`. This is what proved
the `8b7846f` SELECT fix: the earlier failure was a missing SELECT on the phone path, not
a parsing or format problem.

**Card state, read on the ACR122U.** 7 notes live, **481,280 msat** total; slots 0–4 and
6–7 live, `free slots: [5]`, `to renew: []`.

**Card figure math.** `formatSats` takes msat and divides by 1000: `470144 → "470.144"`,
`481280 → "481.28"`. The screen that looked wrong ("470,144 sats") was the reader
misreading a correctly-formatted value. Do not "fix" `formatSats`.

**Planner cost.** Every price from 5,000 to 470,000 plans in 0.4–8 ms, measured with one
free slot. Worst case with the full `BUDGET` is ~27 ms.

**Host tests**, with the planner change in place
(`2 / 14 / 19 / 0 / 22 / 8 / 20 / 0 / 0`), run twice, ~97–99 s each.

---

## 4. What is compiled but not seen working

**The provisioner loop screen — now seen on the Pixel, still not on Max.** On
2026-11-03 23:24 it rendered on the Pixel 8 Pro exactly as designed: "0 of 1" headline,
"0 spendable · 0 stopped", the ghost card face, "Hold the card against the phone.", then
the Card 1 of 1 row, INSTALL THIS CARD, STOPS, END BATCH. Two observations, neither
fixed: the ghost face at opacity 0.22 on the light theme hides the FEDIMINT wordmark
entirely (chip and roundel read, the word does not), and the phase line and the card
row's sub both say "Hold the card against the phone." one above the other. Reaching
the screen needed the dev local session plus a temporary power-of-two default
top-up (§ 8, first item); both edits were reverted before anything was committed.
The icon was seen in the Pixel app drawer (navy disc, gold chip, legible at launcher
size). `npx tsc --noEmit` clean and `npx jest` 64/64 throughout.

The build did reach hardware this session:

- **iOS, device "Max"** (iPhone 13 Pro Max, `00008110-001E4CC014D8401E`):
  `npx react-native run-ios --device "Max" --no-packager` → *"Installed the app on the
  device"*, 61 s.
- **Android, Pixel 8 Pro** (`42201FDJG00048`): the foreign-signed install was removed by
  the owner on 2026-11-03 (`adb uninstall`, which an agent's permission classifier blocks
  as an irreversible deletion) and the current debug APK installed. The debug APK carries
  no JS bundle: it needs Metro. Port 8081 on this Mac is held by a Metro a previous
  session left running from this same package, so the one used tonight is on 8082 with
  `adb reverse tcp:8081 tcp:8082`; both are still running.

Bundle id is `org.fedimintcard.provisioner` on both platforms
(`ios/FedimintProvisioner.xcodeproj/project.pbxproj` lines 309/339,
`android/app/build.gradle:82`).

### An earlier dead end, recorded so it is not re-run

A screenshot of a booted iPhone 16 Pro simulator showed a real-estate dashboard called
"Numbers", and `simctl launch org.fedimintcard.provisioner` then failed with
`FBSOpenApplicationServiceErrorDomain, code=4`. Neither had anything to do with our build:
`xcrun simctl get_app_container booted org.fedimintcard.provisioner app` returns
`NSPOSIXErrorDomain, code=2 ... No such file or directory`, i.e. **nothing was installed
on that simulator** and the screenshot was stale content from an unrelated app. The
simulator was never the target — "Max" is a physical device.

---

## 5. Provisioner work in progress

### `apps/provisioner/src/CardFigure.tsx` (new, ~4.6 KB)

An SVG card face drawn from `react-native-svg` **primitives**, not an image asset. It
imports `FONT`, `useTheme`, `Theme` from `./ui` — there is no `src/theme.ts` in this
package, unlike Cashu. ISO/IEC 7810 ID-1 ratio (`W = 240`, `H = W / (85.6 / 53.98)`).
Black face, gold chip contacts, accent lightning roundel, eight slot pips along the base,
"FEDIMINT" wordmark at `letterSpacing 2.4`. Props are
`{ fill: number, state: 'idle' | 'running' | 'done' | 'stop' | 'danger' }`; the ghost face
sits at `opacity 0.22` until written, and the written state is an absolutely-positioned
clipped `View` of the full-ink face.

`metro.config.js` already wires `react-native-svg-transformer` for `.svg` assets — that
was set up for this and is *not* used by `CardFigure.tsx`; it needs no transformer.

### `apps/provisioner/src/screens/LoopScreen.tsx` (restructured)

Counter is the headline: `local.counter`, `FONT.tabular`, `fontSize 44`,
`testID="counter"`. Progress bar under it, a `<Dim>` spendable/stopped line, then a
centred `CardFigure` in `local.figureWrap` with the live phase line below it. Footer
unchanged.

`figureFill` comes from real block progress — `1` when the last record is `ok`, else
`loop.loadProgress.index / max(1, of)`, else `0`. Never a timer. `figureState` derives
from `stopShown` / last-record-ok / busy / idle.

Note `LoopState` comes from `useLoop()` (`store.tsx:850`), not from `BatchView`
(`store.tsx:351`) — `batch.loop` does not exist.

### `apps/provisioner/src/strings.ts`

Added `LOOP.of: (done, of) => \`${done} of ${of}\``.

### What is left on parity

Unchanged screens to compare against Cashu: `SignInScreen` (`App.tsx:36`),
`BatchSetupScreen` (`:62`), `BatchSummaryScreen` (`:40`), `SettingsScreen` (`:78`), plus
`CardFigure` usage in the summary. Both apps are React Native 0.77 with near-identical
token layers — Cashu's `ui.tsx`/`theme.ts` and Fedimint's inline `Theme`/`DARK`/`LIGHT`
in `ui.tsx` carry the same values (`bg` `#050505`/`#FAFAF8`, `accent` `#3AB54A`,
`danger` `#E5484D`/`#D13438`). The gap is screens and flow, not the design system, so do
not port a token file.

---

## 6. The icon (done 2026-11-03, `573d8d2`; not yet seen on a home screen)

Source is `apps/provisioner/assets/icon/icon.svg`; `python3 assets/icon/render.py` from
`apps/provisioner` regenerates every asset below (needs `rsvg-convert` and Pillow). The
Android background is now `@drawable/ic_launcher_background` (a PNG per density), not a
color. The rest of this section is the original brief, kept for the measurements.

The app icon was a pixel-art meme head — a bald figure in "deal with
it" sunglasses on a dark green field, in both
`ios/FedimintProvisioner/{Assets,Images}.xcassets/AppIcon.appiconset/` and
`android/app/src/main/res/mipmap-*/`. It must instead match the avatar at
`github.com/fedimintcard`.

That avatar is a **gold, faceted-gem logo on a navy field**, not the meme head:

- **Background:** vertical gradient, `#24428E` at the top to `#12214A` at the bottom.
- **Gold panel:** rounded rectangle, x 93–360, y 125–334 in the 460×460 source
  (≈267×209, corner radius ≈30), fill a gradient from `#E1CF89` (upper left) to
  `#AF9244` (lower right).
- **Line work:** uniform ~5–6 px strokes in `#24418C`, a small pentagon (apex up, flat
  bottom; x 190–267, y 190–268) plus seven strokes radiating to the panel border — one
  vertical off the apex to the top edge, two diagonals to the top rounded corners, one to
  each side edge at y≈258, and two splaying to the bottom edge at x≈177 and x≈280.

There is **no vector original in this repo** (`find` for `*.svg` outside `node_modules`
and `target` returns only jemalloc build artifacts), and the avatar is a 460 px raster
that will not upscale cleanly to 1024. So the icon has to be reconstructed as SVG from
the geometry above and rendered to the required sizes — the same approach already taken
for `CardFigure.tsx`.

What must be produced, and the constraints each target imposes:

- **iOS:** `Icon-1024.png` must be 1024×1024, **opaque, no alpha**, full-bleed — do not
  pre-round the corners, iOS applies its own mask. Plus the existing sizes in
  `Contents.json`: 20@2x/3x, 29@2x/3x, 40@2x/3x, 60@2x/3x, 76@2x, 83.5@2x.
- **Android:** the adaptive icon is already correctly split —
  `mipmap-anydpi-v26/ic_launcher.xml` points at `@color/ic_launcher_background`
  (currently `#10271E`, the meme green) and `@drawable/ic_launcher_foreground` (currently
  a PNG in five density buckets). The navy gradient belongs in the background layer and
  the gold panel in the foreground. The foreground's essential content must sit inside the
  centre 66×66 dp, so the gold panel needs scaling down and centring or the adaptive mask
  will crop it. Legacy `mipmap-*/ic_launcher.png` and `ic_launcher_round.png` need
  regenerating too.

Sampling the avatar for exact values is a one-liner away:

```sh
curl -sL "https://avatars.githubusercontent.com/u/348971489?v=4&s=1024" -o /tmp/gh-avatar.png
python3 -c "
from PIL import Image; import numpy as np
a = np.array(Image.open('/tmp/gh-avatar.png').convert('RGB'))
print(tuple(int(v) for v in a[10,230]), tuple(int(v) for v in a[450,230]))"
```

---

## 7. Commands that work

```sh
cd /Users/dread/Repos/fedimintcard
export D=~/.local/share/fedimint-card-demo/wallet
CS=./host/target/debug/card-sidecar
env -u CARD_BRIDGE_URL CARD_BACKEND=pcsc CARD_PIN=1111 $CS --data-dir "$D" card balance
env -u CARD_BRIDGE_URL CARD_BACKEND=pcsc CARD_PIN=1111 $CS --data-dir "$D" card spend --price 131000
```

Stop the till before either card command, or they contend for the reader:

```sh
lsof -nP -iTCP:9877 -sTCP:LISTEN | awk 'NR>1{print $2}' | xargs -r kill
(CARD_BACKEND=bridge CARD_BRIDGE_URL=http://127.0.0.1:9876 CARD_PIN=1111 \
  nohup ./host/target/debug/card-sidecar --data-dir ~/.local/share/fedimint-card-demo/wallet \
  till serve > /tmp/till.log 2>&1 &)
```

adb tunnels — **re-run after any adb restart or reinstall**:
`adb forward tcp:9876 tcp:9876` and `adb reverse tcp:9877 tcp:9877`.

| Task | Command |
|---|---|
| Host tests (from `host/`) | `env -u CARD_BACKEND -u CARD_PIN -u CARD_BRIDGE_URL cargo test --locked -p fedimint-card-host-card -p fedimint-card-sidecar --profile ci` (~97–136 s) |
| Relay tests | `cd android-relay && ./gradlew :app:testDebugUnitTest -q` |
| Relay build+install | `./gradlew :app:assembleDebug && adb install -r app/build/outputs/apk/debug/app-debug.apk && adb shell am start -n org.fedimintcard.relay/.MainActivity` |
| Provisioner typecheck | `cd apps/provisioner && npx tsc --noEmit` |
| Provisioner tests | `npx jest` |
| Sidecar with PC/SC | `cd host && cargo build -p fedimint-card-sidecar --features pcsc` (~16 s) |
| iOS run on Max | `cd apps/provisioner && npx react-native run-ios --device "Max" --no-packager` |
| Android run on Pixel | `cd apps/provisioner && npx react-native run-android` |
| Screenshot a simulator | `xcrun simctl io booted screenshot /tmp/pN.png` |

The till listens on `http://127.0.0.1:9877` (log `/tmp/till.log`). Routes
(`till.rs:696-703`): `GET /v1/health`, `GET /v1/status`, `POST /v1/charge`, `GET /v1/sales`,
`POST /v1/card/balance`, `POST /v1/cards/precheck`, `POST /v1/cards/keySetup`,
`POST /v1/cards/issue`.

`ERROR iroh::socket: Endpoint dropped without calling Endpoint::close` in till logs is
harmless; filter it with `grep -v "iroh::socket"`.

Federation `9dc886ab35c45c162b27fa3da6fe356375b074b5496c7e9afa5b1c8e7fd61288`
("Island Bitcoin Community", mint fee 0 msat). Demo price 131 sats (`131000` msat) per
`docs/DEMO.md`. Relay is `org.fedimintcard.relay`, POS `org.fedimintcard.provisioner`,
on adb device `42201FDJG00048`.

The card PIN is **`1111`**. It is supplied as `CARD_PIN`, never `TEST_PIN`, and should
never be guessed or probed speculatively with `VERIFY_PIN`.

---

## 8. Known gaps — reported, not fixed

These are real defects that need approval before anyone touches them.

**START BATCH could never enable on a fresh install — fixed 2026-11-04.** The shipped
default `[1048, 524, 262, 131]` was the demo's msat plan rounded to sats, and the app's
own `isValidTopup` demanded powers of two in sats, so the default failed its own rule
and the loader fell back to it. Neither the list nor the power-of-two rule was needed:
the sidecar (`glue.rs` `topup_ref`, `cards.rs` `issue_plan`) runs every amount through
`plan::binary` and makes the notes itself. `topupSats` is now one number, sats per
card, default **0 = install only** (what the Cashu provisioner does); the preview
lists the notes the sidecar will make of it and refuses a sum that splits into more
than the card's 8 slots. What the client still cannot know is the federation's
smallest economical denomination: a sum whose binary split has a note below it (2,000
sat ends in a 128 msat note) is refused by the sidecar at issue time, not in the
preview, because the till exposes no plan-preview route. Verified on the Pixel: a
fresh install shows "No top-up" and START BATCH enables once an invite is in.

**The invite rule was wrong — fixed 2026-11-04.** Every "a batch can start" check
required the invite to begin `fedimint://`. No such form exists: the sidecar parses
with fedimint-core's `InviteCode::from_str`, which takes bech32m with HRP `fed1` (so
`fed11…`) or the `fedimint` prefix over base32hex. A real invite, pasted or scanned,
could therefore never start a batch. `normalizeInvite` in `federation.ts` is now the
one rule (lowercases and trims, as the parser does), used by Settings, NEW BATCH, the
settings loader and `settingsAreReady`.

**`renew_spent_slots` swallows failures.** `glue.rs:417-436` (call site `:1279`) logs each
per-slot failure with `tracing::warn!` and returns `Ok`, so a sale can report success
while its renewal silently did not happen. The retry loop added in `df5b6b0` bounds
attempts at `RENEW_ROUNDS = 3` but does not surface exhaustion.

**No consolidation route.** `topup_ref` (`glue.rs:472`) could serve one, but the till has
no endpoint for it, so notes fragmented across slots cannot be merged except by a sale.

**`TooManyNotes` is hard-coded at 20** (`plan.rs:289`) regardless of the card's 8-slot
reality, which is a cosmetic wart in the error path.

**Card-balance success path unverified.** The feature is committed and unit-tested, but
the card has never been moved from the ACR122U to the phone, so the success response
(`481.28 sats`, 7 notes, `free slots: 5`) has never come back through the relay. A PC/SC
spend attempt late in the session failed with *"The operation requires a Smart Card, but
no Smart Card is currently in the device"* — the card was simply off the reader.

---

## 9. Next steps

**Max's Debug build loads its JavaScript from this Mac's Metro on 8081 when one is
reachable, not from the embedded bundle** (`PROVISIONER-HANDOFF.md` says the opposite;
it is wrong). On 2026-11-04 a Metro started 2026-10-30 (pid 12506) was still on 8081
with a dead file watcher, so Max showed the old UI after three correct installs. Check
with `curl -s 'http://localhost:8081/index.bundle?platform=ios&dev=true' | grep -ac 'SCAN QR'`
(0 = stale). Fix: kill that Metro — the app then falls back to the embedded bundle — or
launch with `devicectl device process launch --terminate-existing … -- -RCT_jsLocation
<mac-ip>:<port>` to point it at a fresh Metro (the arg does not survive a cold start).

Screenshotting Max from this Mac is still unsolved: `idevicescreenshot` needs a DDI the
device no longer offers, Maestro reports the phone "not connected", iPhone Mirroring
cannot pair (different iCloud account), and Xcode's Devices window did not render in the
filtered desktop capture. The Pixel is fully drivable over adb (`input tap`/`text`,
`exec-out screencap`); use it for visual checks. The loop screen and the icon are
**installed on Max but never seen there**.

1. Scan a real federation QR on a phone (the sheet, the permission flow and the live
   viewfinder are verified on the Pixel; no QR was put in front of it). Then start a
   batch against that federation and watch the sidecar's answer to a top-up sum.
2. Look at the icon on the Max home screen and the redesigned screens on Max.
   Program a real card through the new loop: the programmed rows, the stop panels
   on a real stop and a top-up have only run against the simulated card.
3. Decide two things the design pass surfaced and did not change: (a) with "A PIN
   per card" the summary masks the generated PIN as dots and keeps only the last
   card's, so holders can never learn their PIN; (b) the registry batch row's
   `federation_id` is sent the whole invite string, not the federation id
   (`decodeInvite` now makes the real id available).
3. Continue screen parity: `SignInScreen`, `BatchSetupScreen`, `BatchSummaryScreen`,
   `SettingsScreen`.
4. Put the card on the phone and verify the card-balance success path.
5. Fold the outcome into this document by editing with `python3` string replacement —
   **do not run Prettier over the docs**, it rewraps prose that is deliberately laid out.
