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

/** What the picker and the summary print for a federation. */
export function federationLabel(federation: Federation): string {
  return federation.label || federation.invite;
}
