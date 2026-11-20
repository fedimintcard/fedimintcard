# Hardware test report: J3R452 and J3R180, 2026-11-15 to 2026-11-19

Status: Install, key rotation, reset to blank and resume after a torn tap passed on one J3R452 and one J3R180 with a
Pixel 8 Pro and an iPhone; the torn taps ran on the J3R452 only. On the J3R452 the staging till then passed a balance
tap, a top-up load, an online charge and an offline charge whose change was finished at the next balance tap. The till
build was a630a22, which is older than main, and no POS phone app took part; no command trace exists for any till step.
Not run: the Cashu Card 2.2 selftest, the daily limit refusal, a release, the day window's restart, the renewal of a
slot spent offline, and the first contact and retap read traces.

## Summary

| Flow | Card | Phone or reader | Result | Date |
|---|---|---|---|---|
| 1 install tap, batch of two | J3R452, J3R180 | Pixel 8 Pro | Pass, Pixel log and screen only | 2026-11-15 |
| 1 install tap, two batches of one | J3R452, J3R180 | iPhone | Pass, no tap times | 2026-11-16 |
| 2 key rotation and escrow across phones | J3R452 | iPhone resets a Pixel install, Pixel resets an iPhone install | Pass | 2026-11-16, 2026-11-18 |
| 3 reset to blank | J3R452, J3R180 | Pixel 8 Pro | Pass, Pixel log and screen only | 2026-11-15 |
| 3 reset to blank | J3R452 | iPhone, blocked PIN path on the second date | Pass | 2026-11-16, 2026-11-18 |
| 3 reset to blank | J3R180 | iPhone | Pass | 2026-11-18 |
| 3 reset to blank | J3R452 | Pixel 8 Pro, card installed by the iPhone | Pass | 2026-11-18 |
| 4 torn tap in the load, then install | J3R452 | Pixel 8 Pro | Pass | 2026-11-15 |
| 4 torn tap in the keying, then finish | J3R452 | Pixel 8 Pro | Pass | 2026-11-18 |
| 4 torn tap in the keying, then finish | J3R452 | iPhone | Pass, no phone-side trace | 2026-11-18 |
| 5 balance tap on an earlier J3R452 install, registry v13 | J3R452 | staging till, USB reader | Pass | 2026-11-15 |
| 5 balance tap, top-up and load, online charge | J3R452 | staging till, USB reader | Pass | 2026-11-18 |
| 5 offline charge, settlement, balance tap that finished the change | J3R452 | staging till, USB reader | Pass | 2026-11-19 |
| 5 limit refusal, release, window restart, renewal of a slot spent offline | J3R452 | staging till | Not run | |
| 6 Cashu Card 2.2 selftest | J3R452 | | Not run | |

## Setup

Commit ids are commits of this repository; main is 957b19b at the time of writing. Flow numbers follow the pass script: 1
install tap, 2 key rotation and escrow, 3 reset to blank, 4 resume after a torn tap, 5 till payment and the offline paths,
6 the Cashu Card 2.2 selftest. Pass means the step completed and every reading taken agrees. Times are the Mac's clock as
the records print it: PST on 2026-11-15 and 2026-11-16, and EST from 2026-11-18, when the Mac's zone had changed. The
Pixel's tap times come from its NFC log (`dumpsys nfc`), which read PST on 2026-11-15 and agreed with the Mac on
2026-11-18. No time is converted. Evidence is named as reader (a raw read-only read through the USB reader, no login),
registry (a read of the staging registry), till (its own answer or status), Pixel log, or screen (a screenshot, or a screen
the operator read out).

| | J3R452 | J3R180 |
|---|---|---|
| Model | NXP J3R452, JCOP 4.5 | NXP J3R180, JCOP 4 |
| ATR as read | 3B8A80014A3352343532444E47520C (historical bytes J3R452DNGR) | 3B8A80014A546178436F7265563150 (historical bytes JTaxCoreV1) |
| CPLC IC type | D600 | D321 |
| Secure channel, card keys | SCP03, three keys at version 01, type 88 (AES) | SCP02, three keys at version 01, type 80 |
| Applications, blank and with ours | 5 and 7 | 1 and 3 |
| Free memory, blank | 209,008 bytes before the first install | 119,508 bytes at its first blank read, 2026-11-18 |

The J3R452 had been moved to the public default key set at version 01 on 2026-11-08, before the pass.

- Phones and apps. Pixel 8 Pro: Setup 1.0.0 (12), a release APK, and POS 0.4.0 (3), a debug build. iPhone (iOS 26.5.2):
  Setup 1.0.0 (12), a Release build with its bundle embedded, and POS 0.2.0 (4), a store build. Both Setup apps were
  built from main 226d75a on 2026-11-13; they carry the SCP03 port (3803a20) and install applet 3.0. Git shows no
  change in applet/, apps/, tools/ and pom.xml between 226d75a and 957b19b. The POS apps took part in no step below.
- Reader. An ACS ACR122U (PICC interface) on the Mac, read with GlobalPlatformPro v25.10.20.
- Till. The staging till on the Mac, built from a630a22 (2026-11-10), offline caps 3 charges and 100,000 msat, reached
  over loopback. Its relay passes commands to the USB reader. Main has 15 later commits in host/sidecar.
- Registry. The staging registry at v13 (Worker version 59559643) since 2026-11-15 08:09 PST. Both Setup apps were
  signed in to it ("Recording to the test registry.").
- Federation and network. The till's federation is Island Bitcoin Community. The top-up invoice was a mainnet Lightning
  invoice, so the staging federation moves real sats. Wi-Fi was the Mac's only routable link and was switched off for
  the offline charge.
- Recording. The Pixel's NFC log keeps headers and sizes without payloads, so its traces show timing and shape only.
  On 2026-11-15 (logged 10:59 PST) the Mac ran critically short of memory and the iPhone syslog recorder was stopped.
  It was not restarted, so the iPhone has no tap times and no phone-side trace.

## Flow 1 and 2: install tap, key rotation and escrow

Pixel 8 Pro, 2026-11-15, one batch of two with no offline limit. The J3R452 was tapped first, then the J3R180. The
screen showed 2 programmed and 0 stopped, each card in one tap.

| Seconds, Pixel log, PST | J3R452, tap at 08:35:26.134 | J3R180, tap at 08:35:42.298 |
|---|---|---|
| Load | 3.356 | 3.252 |
| Instantiate | 1.202 | 0.982 |
| Key rotation | 0.485 | 0.472 |
| Keying | 2.364 | 2.445 |
| Whole tap | 8.630 | 8.258 |
| Inside the card | 7.445 | 7.126 |

First activation to the last app exchange read 8.640 s and 8.269 s. The registry waited 145 ms for the manager keys (the
escrow row is written before any card write) and 167 to 187 ms for the card certificate. The phase labels come from a
code reading of main 76f7b1e that maps every app exchange of both taps one to one. The rotation is an in-place key
change at version 01, followed by a login that proves the new set (size level). The Pixel's installs have no reader
read: the J3R452 was reset in flow 3 before the first reader read of an installed card (flow 4), and the J3R180 the
Pixel installed was never read on the reader.

iPhone, 2026-11-16, two batches of one with an offline limit of 1000 sat, so the step from card 1 to card 2 was not
exercised and no tap time exists. J3R452, reader at 01:15:28 PST: applet 3.0, id dda3340f, slots 0 to 7 keyed and 8 to
31 empty, limit 1,000,000 msat, spent 0, certificate, attestation key and registry key set, 182,128 bytes free;
registry: version 0, 8 keyed, no notes. J3R180, reader at 10:46:19 and 10:47:54 EST on 2026-11-18: id c8c4bc65, its
state byte for byte the J3R452's, 92,992 bytes free; registry at 10:48:37 EST agrees. Pass.

Escrow across phones. The iPhone reset the J3R452 the Pixel had installed (2026-11-16), and the Pixel reset the J3R452
the iPhone had installed (2026-11-18). Each reset needed the manager keys the installing phone had put in escrow. Pass.

## Flow 3: reset to blank

Pixel 8 Pro, 2026-11-15, Pixel log, PST. Both cards were reset with two taps each, the first read only. The J3R452
(id ending 4735) had its second tap at 10:02:16.137, 2.711 s; the J3R180 (ending 9145) at 10:02:39.928, app part 1.938 s.
The screen showed "Card ...9145 is reset" and that ops had the reset. The trace shows the key set restored in place
and proved by the next login (size level). No reader read and no registry read is recorded for these two resets. Pass.

Pixel 8 Pro, 2026-11-18, a J3R452 the iPhone had installed (id e3a1dd58). Pixel log, EST: "Read the card" tap at
12:43:11.671, 0.262 s; "Reset this card" tap at 12:43:18.267, 3.278 s, the same shape as the 2026-11-15 reset. Registry at
12:45:15 EST: reset recorded. Reader at 13:59:13 EST: applets absent, 208,656 bytes free. Pass.

iPhone. J3R452, 2026-11-16: the operator reported the reset at about 00:41 PST; reader at 00:45:58 PST: blank, 208,656
bytes free (182,128 installed); registry: reset recorded. J3R452, 2026-11-18, between a registry read at 11:02:45 EST
(live) and a reader read at 11:09:59 EST (blank, 208,656 bytes free): wrong PIN entries blocked the PIN, and the reset ran
down the blocked-PIN path ("I am sure: reset without the PIN"), the first run of that path on a real card that the
records know of; registry at 11:13:22 EST: reset recorded. J3R180, between registry reads at 10:48:37 EST (live) and
11:13:22 EST (reset recorded): the PIN worked at the first try and the reset ran the normal way; reader at 12:45:14 EST:
1 application, 119,508 bytes free, against 92,992 installed. No iPhone phone-side trace exists. Pass.

## Flow 4: torn tap and finishing tap (J3R452 only)

Pixel 8 Pro, 2026-11-15, a batch of one with no limit, tear in the load (Pixel log, PST). The card was in the field from
14:33:55.145 to 14:33:57.056 (about 1.9 s); the 13th of 41 load blocks was sent and never answered. The screen showed "Card
left early" and the card unchanged, with the batch's only slot used. Two retaps (14:35:16.982, 14:35:34.078) read the
card's NDEF and the app sent nothing. In a new batch of one the finishing tap was in the field at 14:39:08.715, one
session to 14:39:17.593. The card kept nothing of the 12 loaded blocks and a full install ran from block 1: a load is
never resumed. Phase split in seconds: probe 0.079, login 0.130, delete 0.084, load 3.541, instantiate 1.201, applet
check 0.211, key rotation 0.482, keying 2.384, certificate 0.311, finish 0.046; total 8.867, inside the card 7.622,
phone, radio and registry 1.245; registry waits 195 ms and 158 ms. The total runs from field entry to the last answer;
the ten phases listed sum to 8.469. The screen showed 1 of 1 programmed (row ending B2E7).
Reader at 14:44:45 PST: the same card, applet version 3.0, id fb59b2e7, slots 0 to 7 keyed and 8 to 31 empty, clock 0,
limit 0, spent 0, certificate, attestation key and registry key set, 7 applications, 182,128 bytes free against 209,008
before (26,880 used), key listing unchanged at version 01. Pass.

Pixel 8 Pro, 2026-11-18, a batch of one with a limit of 1000 sat, lifted when the line read "Making the card's keys"
(Pixel log, EST). One session from 14:29:57.621 to 14:30:05.351 (7.730 s); the second key generation was sent and never
answered. The screen showed "Card not finished" and that it "did not use up a place in this batch". Reader at 14:31:09
EST: applet present, 1 slot keyed (id d1bae203), limit 0, no certificate, attestation key not made, 186,616 bytes free;
registry: no row (404). Finishing tap, 14:44:26.124 to 14:44:30.841 (4.717 s), the resume path: a login on the card's own
keys, key generation on slots 0 to 7, no key change. The screen showed 1 of 1 programmed, row ending D2C4. Reader at 14:45:15 EST: id
7fe0d2c4, 8 keyed and 24 empty, limit 1,000,000 msat, spent 0, certificate and keys set, 182,128 bytes free; registry at
14:47:17 EST: live, version 0, 8 keyed, no notes; d1bae203 still 404. Pass.

iPhone, 2026-11-18, no phone-side trace. First try (about 11:55 EST): "Card stopped"; reader at 11:56:18 EST: no applet,
208,852 bytes free, nothing to finish. Second try (about 12:08 EST): "Card not finished", "Tap the same card again" (operator).
Reader at 12:09:36 EST: both applets, 2 slots keyed (id 8393f048), limit 0, no certificate, 186,236 bytes free; registry:
404. Finishing tap (after 12:14 EST): 1 of 1 programmed, row ending DD58 (operator). Reader at 12:16:48 EST: id e3a1dd58,
8 keyed and 24 empty, limit 1,000,000 msat, spent 0, certificate and keys set, 181,748 bytes free; registry at 12:17:33
EST: live, version 0, 8 keyed; 8393f048 still 404. Pass.

## Flow 5: the staging till on the J3R452

Card 7fe0d2c4 is the J3R452 as the Pixel finished it on 2026-11-18. The operator typed the PIN for charges in their own
terminal window; it appears in no record.

1. Balance tap on an earlier install of the card, 2026-11-15 14:45:56 PST, till a630a22 against registry v13. HTTP 200 in 1.16 s: id
   fb59b2e7, 0 msat, free slots 0 to 9. Reader at 14:46:12 PST: slots 0 to 9 keyed (two spare slots keyed by the tap)
   and the card clock set to 14:33:02 PST, a block header's time. Registry: version 1, 10 keyed. Pass.
2. Balance tap, 2026-11-18 15:15:19 EST: HTTP 200 in 1.70 s, 0 msat, free slots 0 to 9. Reader at 15:15:53 EST: 10 keyed
   (slots 8 and 9 new), clock set (it read 0), limit 1,000,000 msat, spent 0, 180,632 bytes free, 1,496 fewer. Pass.
3. Top-up and load. At 15:16:41 EST the till made a top-up for 100,000 msat and rounded it to 100,352 msat in 2.04 s, as
   [TOPUP.md](TOPUP.md) describes. The poll read waiting at 15:16:58 EST and paid at 15:20:38 EST. The load, sent at
   15:22:54 EST with no PIN, answered HTTP 200 in 4.54 s, state loaded. Reader at 15:23:22 EST: slots 0, 1 and 2 NOTE,
   3 to 9 keyed, 10 to 31 empty, 180,632 bytes free (a note takes no memory). Registry: version 3, three live notes of
   65,536, 32,768 and 2,048 msat, 100,352 msat live. The till's wallet read the same before and after. Pass.
4. Online charge, 16:10:23 EST, 2,000 msat, reference eng839-charge-1: HTTP 200 after 6.7 s (charge tool), the till's own
   figure 3,454 ms; sale 31; one input, slot 2 at 2,048 msat; no change; overpaid 48 msat; fees 0. The till's wallet went
   up by exactly 2,048 msat. Reader at 16:12:04 EST: slots 0 and 1 NOTE, 2 to 11 keyed (slot 2 re-keyed with a new key, 10 and 11 new),
   clock and window start 16:05:12 EST, spent in window 2,048 msat, 179,136 bytes free. Registry: version 8, 12 keyed, two
   live notes, 98,304 msat live. Every field matched the prediction made from the planner code. Pass.
5. Offline charge, 2026-11-19 09:43:35 EST, Wi-Fi off, 16,000 msat, reference eng839-offline-1. The tool's guard read the
   till's status first and found it cut off from its registry or federation. HTTP 200 after 2.0 s (tool), the till's
   figure 1,863 ms: offline true, received 0, collection pending, input slot 1 at 32,768 msat, change slot 2 at 16,384
   msat, overpaid 384 msat, fees 0, sale 32. Every field matched the prediction; the change slot's number was not predicted.
6. Settlement, Wi-Fi back. Till at 09:44:19 EST: busy, queue 1 at 16,384 msat. At 09:44:44 EST: queue 0, the wallet up by exactly
   16,384 msat, nothing double spent, refused or rejected, no restart. Ledger: 32 sales, 6 offline, 0 pending.
   Reader at 09:44:57 EST: slot 0 NOTE, slot 1 SPENT, slot 2 CHANGE, 3 to 11 keyed, spent in window 34,816 msat (2,048
   plus 32,768), card clock moved to 09:29:50 EST, 179,136 bytes free. Registry at 09:45:17 EST: version 10, live 81,920
   msat (slot 0 at 65,536, slot 2 at 16,384), slot 1 spent. Pass.
7. Balance tap that finished the change, 09:58:30 EST: HTTP 200 in 1.37 s, live 81,920 msat, free slots 3 to 12. Reader at
   09:58:47 EST: slots 0 and 2 NOTE, slot 1 SPENT, no CHANGE, 3 to 12 keyed, clock 09:52:47 EST, spent in window 34,816
   msat, 178,388 bytes free (748 less, one slot keyed, as predicted). Registry at 09:58:49 EST: version 11, 13 keyed, live
   81,920 msat. Wallet unchanged, queue 0. Pass.

End state: the J3R452 holds two notes worth 81,920 msat of real value on the staging federation and must not be reset
while it does. Slot 1 stays SPENT until a later tap that carries a PIN.

## Findings

1. Free memory. A blank J3R452 read 209,008 bytes before the first install, 208,656 after each of three full resets and
   208,852 after a stopped tap, so the blank figure depends on what last touched the card; 352 bytes went once, early in
   the pass, and the cause is not pinned. The J3R180's blank figure was read once (119,508), so whether it varies is unknown.
2. The iPhone's resumed install ended with 181,748 bytes free against 182,128 for a one-tap install, while the Pixel's
   ended at 182,128. A model of 748 bytes per keyed slot's key pair fits the readings at 8, 10, 12 and 13 keyed slots and
   puts a card with all 32 keyed at 164,176 (not read).
3. The 380-byte gap fits one key object orphaned by a tear inside the applet's key allocation
   (FedimintCardApplet.java:668-682); this is inferred, and a full reset gave the bytes back. A re-key and a note write
   cost no memory.
4. A torn card in a batch of one uses the batch's only slot, the app stops listening, and its stop text still promises
   a next tap (LoopScreen.tsx:394-396, strings.ts:159). Whether a card that left unchanged should use a slot is open.
5. The batch PIN is typed once in a masked field with no confirm entry (BatchSetupScreen.tsx:80, :266-280), and a typing
   slip fits the J3R452's blocked PIN but is not proven. After a wrong PIN the reset screen adds "The card is unchanged."
   although a try was spent (strings.ts:350, :362; ResetScreen.tsx:245-249).
6. The batch summary prints tap times in UTC (BatchSummaryScreen.tsx:35-36): the Pixel's taps at 08:35 PST read 16:35.
7. By the code a lift keeps the card's place only in the keying, in the PIN step or while resuming (installTap.ts:155-158);
   the iPhone's first try ended "Card stopped" on a blank card, so it stopped before the applet was installed, and its name
   line was not read out. After a lift in the keying the finishing tap gave the card a new id (8393f048 became e3a1dd58,
   d1bae203 became 7fe0d2c4), and the half-state ids never reached the registry.
8. The till wrote no log line for either charge, and neither its log nor the relay's holds a PIN word or a hex run. After
   the offline charge the till's federation client recovered from a full network loss without a restart, its status
   route answered with the network off, and the relay connected to the reader again on the till's first command
   after the reader had been unplugged and plugged in again on 2026-11-18.
9. The till's own 2 s registry check missed once (2026-11-19 09:55:55 EST, registry_reachable false) while three direct
   requests answered HTTP 200 in under 0.4 s; reads from 09:56:12 to 09:56:31 EST showed both reachable. A field till has
   shown the same kind of miss (ENG-886).
10. The registry's row for slot 2 still carries a blinding key after the note reached the card (read at 09:58:49 EST).
    Whether the registry should clear it was not checked.
11. Code reading without a look at the screen: the iOS scan sheet is set only by the tap's own steps (LoopScreen.tsx:224-229), so
    it reads "Reading the chip" through login, load and install, while load progress reaches the app's own line only
    (LoopScreen.tsx:262-280; session.ts:200-203). The operator did not remember what the sheet showed.
12. Stale text about an older, smaller load file remains in apps/provisioner/src/cardkit.ts:10 and :25, settings.tsx:81
    and docs/CAP-BUILD.md:25-26. The pinned file is larger (tools/cardkit/gp/load-file.ts:9); the taps sent 41 load blocks where that text says 23.
13. Corrections made during the pass: the J3R180's installed free memory is 92,992 bytes (an earlier note said 93,056); a spent slot's
    renewal after an online charge was seen (slot 2, 2026-11-18), so only the renewal of a slot spent offline is unseen; an
    inference that the federation's smallest note is 2,048 msat was withdrawn; the pass script wrongly lists SET_CERT as
    till-driven.

## Evidence

The raw records are kept privately by the operator: the running session log, reader reads, registry reads, till answers
and status reads, charge answers, the Pixel's NFC logs and screenshots. They hold command traces and card state that are
not published.

## Not covered

- The till flow ran on the J3R452 only. The torn taps and their finishing taps ran on the J3R452 only, on both phones,
  and so did the proof of escrow across phones. The J3R180 had no torn tap and no till step.
- The Cashu Card 2.2 selftest on the J3R452 (flow 6) was not run. The card holds notes and must not be reset, and the
  records place the selftest after emptying and resetting the card.
- Through the till on real hardware none of these ran: the daily limit refusal, a release of held slots (RELEASE,
  [OFFLINE-PROTOCOL.md](OFFLINE-PROTOCOL.md), "Releases"), the day window's restart, the renewal of a slot spent
  offline. A run of the limit refusal on the J3R180 (limit 70 sat, a 100 sat top-up, three charges) was designed and never run.
- The acceptance plan lists SET_CERT among the paths a till tap drives. Only the Setup app sends SET_CERT, in the install
  tap (apps/provisioner/src/cardSetup.ts:518; installTap.ts:10-11); host/sidecar/src and host/pos-core/src have no
  caller of `set_cert` at 957b19b, so no till step tested it.
- The ENG-838 traces were not all captured. The first contact did not occur because the card was already at key version
  01. The retap read was missed by 20 to 350 ms on 2026-11-18. A J3R452 that kept its factory key set was not tested.
  The in-place key change under SCP03 and the DELETEs were seen at size level only.
- No command trace exists for the offline charge, the online charge, the load or any balance tap. What the card did there
  is inferred from its state before and after and from the code path, including the PIN check, ATTEST, SET_CHANGE,
  SET_NOTE, SET_HEADER and the key generation.
- Two POS phone checks never ran on a device (ENG-841): the count cap refusal, and the kit expiry on the Pixel. No POS
  phone app took part in a payment; the till was driven over its HTTP API on the Mac with the card on the USB reader.
- The iPhone has no tap time and no phone-side trace, and its batch step from card 1 to card 2 was not exercised.
- The Pixel's 2026-11-15 installs and resets of the two flow 1 cards (ids ending 4735 and 9145) have no reader read and no
  recorded registry read. The J3R180 the Pixel installed was never read on the reader.
- A lift during the key rotation itself was not tried. Whether a torn key change leaves a whole key set is not verified,
  and the only J3R452 was not put at risk.
- The reset by chip of a card that was never finished ([OPS-PLAN.md](OPS-PLAN.md), "By chip") was not run. Every torn
  install in this pass either left the card blank or was finished by a second tap.
- Main's 15 later commits in host/sidecar, such as the stranded-slot reporting (76f7b1e), were not run on a card.
