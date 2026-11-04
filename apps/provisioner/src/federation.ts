/**
 * The federations the picker offers, and the type it hands out.
 *
 * Empty, and deliberately so: this repo records no invite string for any
 * federation it ships. `docs/DEMO.md` rehearses against the Island Bitcoin
 * Community federation but names no URL, and a checked-in invite for a
 * federation the operator does not run would send a card's notes to the wrong
 * issuer. The picker therefore starts with only "custom", and
 * `settingsAreReady` refuses to start a batch until the operator supplies one
 * (plan § 5: a second federation is expected, so the field is required, not
 * defaulted). Adding a preset here is a one-line change once a real invite is
 * checked in.
 */

/** A federation as the picker and the summary name it. */
export interface Federation {
  /** The invite, as `fedimint-client` takes it. */
  readonly invite: string;
  /** What the picker and the summary call it; the invite when it has no name. */
  readonly label: string;
}

/** The presets. Empty until an invite is checked in (see the header). */
export const KNOWN_FEDERATIONS: readonly Federation[] = Object.freeze([] as const);

/**
 * The picker's options: the operator's saved federation first, then every other
 * preset, deduplicated. A saved custom invite therefore stays visible and
 * selected without being lost the moment a preset is tapped.
 */
export function federationOptions(saved: string): readonly Federation[] {
  if (!saved) {
    return KNOWN_FEDERATIONS;
  }
  const rest = KNOWN_FEDERATIONS.filter(f => f.invite !== saved);
  return [{invite: saved, label: saved}, ...rest];
}

/**
 * The invite as the sidecar's `InviteCode::from_str` (fedimint-core
 * `invite_code.rs`) will take it, or null. Two encodings exist: bech32m with
 * the human-readable part `fed1` (so the string opens `fed11…`), and the
 * newer `fedimint` prefix over RFC 4648 base32hex. Both are case-insensitive
 * and the parser lowercases, so this does too; surrounding whitespace, as a
 * QR code or a paste may carry, is dropped. There is no `fedimint://` form.
 */
export function normalizeInvite(text: string): string | null {
  const s = text.trim().toLowerCase();
  if (/^fed11[qpzry9x8gf2tvdw0s3jn54khce6mua7l]{6,}$/.test(s)) {
    return s;
  }
  if (/^fedimint[0-9a-v]{6,}$/.test(s)) {
    return s;
  }
  return null;
}

/** Whether `text` is a federation invite the sidecar can join. */
export function isInviteCode(text: string): boolean {
  return normalizeInvite(text) !== null;
}

/** What an invite says about its federation, decoded on the phone. */
export interface InviteInfo {
  /** The federation id, hex: what the sidecar's status reports too. */
  readonly federationId: string;
  /** Each guardian the invite names: its peer number and API address. */
  readonly guardians: readonly {peer: number; url: string}[];
}

const BECH32 = 'qpzry9x8gf2tvdw0s3jn54khce6mua7l';
const BASE32HEX = '0123456789abcdefghijklmnopqrstuv';

/** 5-bit groups to bytes, dropping the incomplete tail (both encodings pad). */
function fiveToEight(groups: readonly number[]): Uint8Array {
  const out: number[] = [];
  let acc = 0;
  let bits = 0;
  for (const g of groups) {
    acc = ((acc << 5) | g) & 0xffff;
    bits += 5;
    if (bits >= 8) {
      bits -= 8;
      out.push((acc >> bits) & 0xff);
    }
  }
  return Uint8Array.from(out);
}

/** A cursor over fedimint-core's consensus encoding (BigSize lengths). */
class Reader {
  private at = 0;
  constructor(private readonly b: Uint8Array) {}
  done(): boolean {
    return this.at >= this.b.length;
  }
  byte(): number {
    if (this.at >= this.b.length) {
      throw new Error('short');
    }
    return this.b[this.at++];
  }
  /** lightning's BigSize: one byte below 0xfd, else a big-endian width. */
  size(): number {
    const first = this.byte();
    const width = first === 0xfd ? 2 : first === 0xfe ? 4 : first === 0xff ? 8 : 0;
    if (width === 0) {
      return first;
    }
    let n = 0;
    for (let i = 0; i < width; i++) {
      n = n * 256 + this.byte();
    }
    return n;
  }
  bytes(n: number): Uint8Array {
    if (this.at + n > this.b.length) {
      throw new Error('short');
    }
    const out = this.b.subarray(this.at, this.at + n);
    this.at += n;
    return out;
  }
}

const hex = (b: Uint8Array) =>
  Array.from(b, x => x.toString(16).padStart(2, '0')).join('');
const utf8 = (b: Uint8Array) =>
  decodeURIComponent(Array.from(b, x => `%${x.toString(16).padStart(2, '0')}`).join(''));

/**
 * The invite's federation id and guardians, read the way fedimint-core's
 * `InviteCode` decodes it: a Vec of parts, each a variant number and a
 * length-prefixed payload — 0 = Api {url, peer}, 1 = FederationId (32
 * bytes); others (an API secret, future parts) are skipped by their length.
 * The checksum is not verified: this is for display, and the sidecar parses
 * the invite for real. Null when the bytes do not read as an invite.
 */
export function decodeInvite(text: string): InviteInfo | null {
  const s = normalizeInvite(text);
  if (s === null) {
    return null;
  }
  try {
    const bytes = s.startsWith('fed11')
      ? fiveToEight([...s.slice(5, -6)].map(c => BECH32.indexOf(c)))
      : fiveToEight([...s.slice('fedimint'.length)].map(c => BASE32HEX.indexOf(c)));
    const r = new Reader(bytes);
    let federationId: string | null = null;
    const guardians: {peer: number; url: string}[] = [];
    const parts = r.size();
    for (let i = 0; i < parts; i++) {
      const variant = r.size();
      const body = new Reader(r.bytes(r.size()));
      if (variant === 0) {
        const url = utf8(body.bytes(body.size()));
        guardians.push({url, peer: body.size()});
      } else if (variant === 1) {
        federationId = hex(body.bytes(32));
      }
    }
    return federationId ? {federationId, guardians} : null;
  } catch {
    return null;
  }
}

/** "9dc8 86ab": the first eight hex digits, grouped for reading aloud. */
export function shortFederationId(id: string): string {
  return `${id.slice(0, 4)} ${id.slice(4, 8)}`;
}

/** How a guardian's address reads on one line: a host, or "iroh node 3b30…". */
export function guardianLabel(url: string): string {
  const iroh = /^iroh:\/\/([0-9a-f]{4})/i.exec(url);
  if (iroh) {
    return `iroh node ${iroh[1]}…`;
  }
  const host = /^[a-z]+:\/\/([^/:]+)/i.exec(url);
  return host ? host[1] : url;
}

/** What the picker and the summary print for a federation. */
export function federationLabel(federation: Federation): string {
  return federation.label || federation.invite;
}
