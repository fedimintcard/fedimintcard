/**
 * The demo card (docs/PROVISIONER-PLAN.md § 5, "Kept deliberately"): a
 * card-side stand-in wired in as a `Transceiver`, so a demo run goes through
 * cardkit's `provision()` exactly as a real one does — same commands, same
 * order, same stop kinds. Nothing here is reachable in a build with demo mode
 * off.
 *
 * Two rules this file keeps:
 *
 * - **The cryptograms are real.** INITIALIZE UPDATE answers a card cryptogram
 *   computed with cardkit's own SCP02 and DES from the profile's key set, so
 *   host authentication is proven the same way on a phone as in this fake. A
 *   wrong key set therefore fails where a real card fails it.
 * - **The framing is this card's.** The Fedimint applet (applet/…/FedimintCardApplet.java)
 *   selects with `02 00` (VERSION_MAJOR/MINOR) and answers CLA `B0`; an unkeyed
 *   slot answers `69 85` and anything else `6D 00`. Cashu's `B0 01` GET_INFO
 *   does not exist here, and neither does a `02 01` version. The provisioning
 *   writes (ISD, INSTALL, LOAD) are the JavaCard's generic ones.
 *
 * Timings are the iPhone column of the 2026-10-18 hardware report, so the demo
 * has the cadence of the thing it stands in for. `speed` scales them; tests use 0.
 */
import {
  CARD_AID,
  FEDIMINT_PACKAGE_AID,
  FACTORY_KEYS,
  ISD_AID,
  OPENER_AID,
  SPIKE_KEYS,
  TransportError,
  equal,
  tdesCbc,
  tdesCbcMac,
  type KeySet,
  type Transceiver,
} from '../cardkit';

/**
 * What the card manager holds when the demo taps it. There is no probe on this
 * card (no GET_INFO), so these are what the *sweep* meets, not a gate:
 *
 * - `none`: a blank J3R180 — DELETE answers `6A 88`, the install writes clean.
 * - `empty`: this card's package installed with no keys — swept and reinstalled.
 * - `old`: the same package at an older version — swept and reinstalled.
 * - `halfWritten`: a torn load left the package behind with no instances. The
 *   sweep cannot see it — DELETE names instances, then the package, and the
 *   package is only reachable once its instances are gone — so
 *   `INSTALL [for load]` answers `69 85` and the run stops `refused`.
 * - `foreign`: the card carries some other applet and answers `6A 82` when ours
 *   is selected, so the sweep is the last phase reached and nothing is written.
 */
export type Installed = 'none' | 'empty' | 'old' | 'halfWritten' | 'foreign';

export interface FakeCardProfile {
  /** What the card manager holds when the tap lands. */
  installed: Installed;
  /**
   * The card manager's key set: `spike` makes the host's login fail, because
   * the card proves keys the host does not hold.
   */
  keys?: 'factory' | 'spike';
  /** During the load, the reply to this LOAD block (1-based) never comes. */
  pullAtBlock?: number;
}

/** Per-command delays (ms), from the iPhone column of the 2026-10-18 report. */
export const DELAYS = {
  plainApdu: 24,
  initUpdate: 50,
  extAuth: 50,
  delete: 325,
  installForLoad: 95,
  loadBlock: 82,
  installOpener: 215,
  installCard: 1280,
  verifyApdu: 25,
} as const;

/**
 * The card's own side of SCP02, from its key set alone: the session keys are
 * 3DES-CBC over `constant ‖ seq ‖ twelve zeros`, and each cryptogram is the
 * full 3DES CBC MAC under S-ENC. Deliberately not cardkit's host helpers
 * (`deriveSessionKeys`, `cardCryptogram`): a card that computed its answers
 * with the code under test would agree with any bug in it.
 */
const SEQ = [0x00, 0x42];
const CARD_CHALLENGE = [0xde, 0xad, 0xbe, 0xef, 0x00, 0x42];

function derive(active: KeySet) {
  const constant = (c: number[]) => [...c, ...SEQ, ...new Array(12).fill(0)];
  return {
    enc: tdesCbc(active.enc, constant([0x01, 0x82])),
    mac: tdesCbc(active.mac, constant([0x01, 0x01])),
    dek: tdesCbc(active.dek, constant([0x01, 0x81])),
  };
}

/** The card's cryptogram: 3DES CBC MAC under S-ENC of host ‖ seq ‖ card challenge. */
const cardCryptogram = (sEnc: number[], host: number[]) =>
  tdesCbcMac(sEnc, [...host, ...SEQ, ...CARD_CHALLENGE]);

/** The host's, which the card checks: 3DES CBC MAC under S-ENC of seq ‖ card ‖ host challenge. */
const hostCryptogram = (sEnc: number[], host: number[]) =>
  tdesCbcMac(sEnc, [...SEQ, ...CARD_CHALLENGE, ...host]);

const sleep = (ms: number) => new Promise<void>(r => setTimeout(r, ms));

const ok = (data: number[] = []) => [...data, 0x90, 0x00];
const sw = (word: number) => [word >> 8, word & 0xff];

/** True when `aid` appears inside `payload` (INSTALL's payload carries its AIDs length-prefixed). */
function containsAid(payload: number[], aid: readonly number[]): boolean {
  for (let i = 0; i + aid.length <= payload.length; i++) {
    if (aid.every((b, j) => payload[i + j] === b)) {
      return true;
    }
  }
  return false;
}

/**
 * The applet's own answer to a selected card, at its version: `02 00` from
 * VERSION_MAJOR/VERSION_MINOR, which cardkit's fedimint plan checks on SELECT.
 */
const APLET_VERSION = [0x02, 0x00];

/**
 * A `Transceiver` wired to a fake card in `profile`. `speed` scales every delay
 * (0 in tests). `installedNow` reports what the fake believes is on the card
 * after the run, for the demo's own assertions and readouts.
 */
export function fakeCard(
  profile: FakeCardProfile,
  {speed = 1}: {speed?: number} = {},
): {tx: Transceiver; installedNow: () => Installed} {
  const active: KeySet = profile.keys === 'spike' ? SPIKE_KEYS : FACTORY_KEYS;
  let installed: Installed = profile.installed;
  /**
   * The card's two independent facts. A blank card has neither; a torn load
   * leaves the package with no instances (`halfWritten`), which is why the
   * sweep's DELETE order — instances first, then the package — cannot clear it
   * when the instances are already gone but the package is not on the list.
   */
  let hasPackage = installed === 'empty' || installed === 'old' || installed === 'halfWritten';
  // Each instance is its own fact: the sweep deletes the card applet and the
  // opener in turn, and a card that lost only one of them still holds an
  // instance, so the package cannot be deleted after it.
  const present = installed === 'empty' || installed === 'old';
  let hasCard = present;
  let hasOpener = present;
  let loadStarted = false;
  let blocks = 0;
  let hostChallenge: number[] = [];
  const at = (ms: number) => (speed > 0 ? sleep(ms * speed) : Promise.resolve());

  /**
   * What each AID answers to SELECT. A half-written package has no instances:
   * the card applet and the opener are gone, the package is still there.
   */
  const hasInstances = () => hasCard || hasOpener;
  const holds = (aid: number[]): boolean => {
    if (equal(aid, CARD_AID)) {
      return hasCard;
    }
    if (equal(aid, OPENER_AID)) {
      return hasOpener;
    }
    if (equal(aid, FEDIMINT_PACKAGE_AID)) {
      return hasPackage;
    }
    return false;
  };

  /** `FF 21 06 82 04 <free u32>`: 164,024 swept, 109,920 once the package is on. */
  const freeMemory = () => {
    const free = installed === 'none' ? 164024 : 109920;
    return [
      0xff,
      0x21,
      // the TLV block's own length: tag 82, length 04, four bytes of value.
      // A byte count that disagrees with the bytes makes the reply unparseable,
      // and the run then reports no free memory at all.
      0x06,
      0x82,
      0x04,
      (free >>> 24) & 0xff,
      (free >>> 16) & 0xff,
      (free >>> 8) & 0xff,
      free & 0xff,
    ];
  };

  /** AID from data, the session C-MAC stripped when wrapped. */
  const aidFrom = (data: number[], wrapped: boolean) =>
    data.slice(0, wrapped ? data.length - 8 : data.length);

  /**
   * The AID inside a DELETE's `4F <len> <aid>` TLV. Every DELETE cardkit sends
   * is shaped this way (gp/scp02.ts `deleteAid`), so matching the raw data
   * would never find the AID and the card would refuse every sweep.
   */
  const deletedAid = (data: number[], wrapped: boolean): number[] => {
    const tlv = aidFrom(data, wrapped);
    return tlv[0] === 0x4f && tlv.length >= 3 ? tlv.slice(2, 2 + tlv[1]) : tlv;
  };

  async function tx(apdu: number[]): Promise<number[]> {
    const ins = apdu[1];
    const wrapped = apdu[0] === 0x84;
    const data = apdu[4] > 0 ? apdu.slice(5, 5 + apdu[4]) : [];

    switch (ins) {
      case 0xa4: {
        // SELECT, always plain
        await at(DELAYS.plainApdu);
        const aid = data;
        if (equal(aid, ISD_AID)) {
          return ok();
        }
        if (equal(aid, CARD_AID)) {
          return holds(aid) ? ok(APLET_VERSION) : sw(0x6a82);
        }
        if (equal(aid, OPENER_AID) || equal(aid, FEDIMINT_PACKAGE_AID)) {
          return holds(aid) ? ok() : sw(0x6a82);
        }
        return holds(aid) ? ok() : sw(0x6a82);
      }

      case 0xca: {
        // GET DATA FF21: plain, before the session
        await at(DELAYS.plainApdu);
        return ok(freeMemory());
      }

      case 0x11: {
        // the applet's GET_PUB (CLA B0): `69 85` until the slot is keyed
        await at(DELAYS.plainApdu);
        if (apdu[0] !== 0xb0 || installed === 'none') {
          return sw(0x6d00);
        }
        return sw(0x6985);
      }

      case 0x50: {
        // INITIALIZE UPDATE
        await at(DELAYS.initUpdate);
        hostChallenge = data;
        const s = derive(active);
        const crypto = cardCryptogram(s.enc, hostChallenge);
        return ok([
          ...new Array(10).fill(0), // diversification data
          0x01, 0x02, // KVN 1, SCP02
          ...SEQ,
          ...CARD_CHALLENGE,
          ...crypto,
        ]);
      }

      case 0x82: {
        // EXTERNAL AUTHENTICATE, already wrapped in its own MAC
        await at(DELAYS.extAuth);
        const s = derive(active);
        const host = hostCryptogram(s.enc, hostChallenge);
        if (!equal(data.slice(0, 8), host)) {
          return sw(0x6982);
        }
        return ok();
      }

      case 0xe4: {
        // DELETE (in session): the applet, then the package
        await at(DELAYS.delete);
        const aid = deletedAid(data, wrapped);
        if (!equal(aid, CARD_AID) && !equal(aid, OPENER_AID) && !equal(aid, FEDIMINT_PACKAGE_AID)) {
          return sw(0x6a88);
        }
        if (!holds(aid)) {
          return sw(0x6a88);
        }
        if (equal(aid, FEDIMINT_PACKAGE_AID)) {
          // the chip refuses this while an instance is still there (6985)
          if (hasInstances()) {
            return sw(0x6985);
          }
          hasPackage = false;
          installed = 'none';
        } else {
          // the sweep deletes the applet, then the opener: only the last one
          // leaves no instance behind
          if (equal(aid, CARD_AID)) {
            hasCard = false;
          } else {
            hasOpener = false;
          }
        }
        return ok();
      }

      case 0xe6: {
        // INSTALL: for-load (P1 02) or and-make-selectable (P1 0C)
        const p1 = apdu[2];
        if (p1 === 0x02) {
          await at(DELAYS.installForLoad);
          // the card refuses a package AID it already holds (6985); the sweep
          // cannot reach a package whose instances are already gone
          if (hasPackage || installed === 'foreign') {
            return sw(0x6985);
          }
          loadStarted = true;
          blocks = 0;
          return ok();
        }
        // A card that is not ours cannot be written: it holds an applet under
        // our AIDs, so INSTALL [for load] has already been refused (6985) and
        // an and-make-selectable is refused the same way. Without this the run
        // would load all 23 blocks and report a foreign card as programmed.
        if (installed === 'foreign') {
          return sw(0x6a80);
        }
        // which instance is arriving decides its cost and the state flips
        const payload = aidFrom(data, wrapped);
        if (containsAid(payload, OPENER_AID)) {
          await at(DELAYS.installOpener);
        } else {
          await at(DELAYS.installCard);
        }
        loadStarted = false;
        installed = 'empty';
        if (containsAid(payload, OPENER_AID)) {
          hasOpener = true;
        } else {
          hasCard = true;
        }
        hasPackage = true;
        return ok();
      }

      case 0xe8: {
        // LOAD
        blocks += 1;
        if (profile.pullAtBlock === blocks) {
          throw new TransportError('the demo card was lifted away');
        }
        if (!loadStarted) {
          return sw(0x6985);
        }
        await at(DELAYS.loadBlock);
        return ok();
      }

      default:
        await at(DELAYS.plainApdu);
        return sw(0x6d00);
    }
  }

  return {tx, installedNow: () => installed};
}
