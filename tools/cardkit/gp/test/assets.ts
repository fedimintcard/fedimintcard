/**
 * The load file the install tests run on.
 *
 * It is normally the real one, generated from the CAP by make-assets.py into
 * gp/load-file.ts and committed. When that file is absent -- a machine
 * where `ant cap` has never run -- this falls back to a placeholder so that the
 * suites which have nothing to do with a load file (the fake card, SCP02, DES)
 * still run. The fallback is loud: it prints, and it is NOT a valid CAP, so any
 * test that depends on the real bytes fails rather than passing on a stand-in.
 *
 * What a placeholder cannot cover: the plan's own guard on the load file's
 * sha256 (provision.test.ts covers that with an asset of its own), and the
 * block count against the real CAP.
 */
import {sha256} from '@noble/hashes/sha256';
import {toHex} from '../bytes';
import type {LoadFileAsset} from '../provision';

/** A whole number of LOAD blocks at the default 247 bytes, so block counts are stable. */
const PLACEHOLDER_SIZE = 247 * 3;

function component(tag: number, body: number[]): number[] {
  return [tag, (body.length >> 8) & 0xff, body.length & 0xff, ...body];
}

/** C4, the BER length, then the components; parseLoadFile reads exactly this. */

/**
 * C4, the length, then a Header and an Applet component. Not a CAP: the fake
 * card reads only the Header's package AID and the Applet list from it, which
 * this supplies, and nothing on the host side parses a load file at all.
 */
function placeholder(): LoadFileAsset {
  const packageAid = [0xf0, 0x46, 0x4d, 0x43, 0x41, 0x52, 0x44];
  const opener = [...packageAid, 0x4d];
  const card = [...packageAid, 0x01];
  // Every length is a byte, and these lengths are written as bytes here: a
  // decimal count in an array of hex bytes is how a file ends up one byte short.
  const u1 = (n: number) => n & 0xff;
  const header = component(0x01, [
    0xde,
    0xca,
    0xbe,
    0xef, // magic
    0x01, // minor
    0x02, // major
    0x00, // flags
    0x00, // package minor
    0x03, // package major
    u1(packageAid.length),
    ...packageAid,
  ]);
  const applet = component(0x03, [
    0x02, // two applets: the opener, then the card
    u1(opener.length),
    ...opener,
    0x00,
    0x00, // install method offset
    u1(card.length),
    ...card,
    0x00,
    0x00,
  ]);
  // The length field counts the components only: C4 and the three length bytes
  // (C4, 82, hi, lo) sit in front of them and are not part of the count. Adding
  // the header to the count as well is how this file once declared three bytes
  // more than it carried, and the card answered 6A80 to the first LOAD.
  const body = PLACEHOLDER_SIZE - 4;
  const filler = body - header.length - applet.length;
  const hexBytes = [
    0xc4,
    0x82,
    (body >> 8) & 0xff,
    body & 0xff,
    ...header,
    ...applet,
    ...new Array(filler).fill(0x00),
  ];
  const hex = hexBytes
    .map(b => b.toString(16).padStart(2, '0'))
    .join('')
    .toUpperCase();
  return {
    // The name is what tells a human this run proved nothing about the CAP.
    name: 'placeholder (no CAP built)',
    size: hex.length / 2,
    // Honest about its own bytes, because provision checks this before the
    // first APDU and refuses to send anything on a mismatch. It cannot be
    // confused with the real CAP's hash: these bytes are not a CAP.
    sha256: toHex(sha256(Uint8Array.from(hexBytes))),
    hex,
  };
}

/** The real load file when make-assets.py has written it, else the loud placeholder. */
export function fedimintLoadFile(): LoadFileAsset {
  let real: LoadFileAsset | undefined;
  try {
    // Loaded lazily: an absent file must be a runtime fallback, not a compile error.
    real = require('../load-file').FEDIMINT_LOAD_FILE as
      LoadFileAsset | undefined;
  } catch {
    real = undefined;
  }
  if (!real) {
    console.log(
      'cardkit: no gp/load-file.ts: run  python3 tools/cardkit/make-assets.py\n' +
        '         (needs applet/target/fedimint-card-0.1.cap; the install tests run on a placeholder)',
    );
    return placeholder();
  }
  return real;
}
