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

/** What the picker and the summary print for a federation. */
export function federationLabel(federation: Federation): string {
  return federation.label || federation.invite;
}
