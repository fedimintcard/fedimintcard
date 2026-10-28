/**
 * The application identifiers the provisioner names. The card's own come from
 * applet/build.xml, which is the only place that states them:
 *
 *   Package AID : F0 46 4D 43 41 52 44      (F0 then "FMCARD")
 *   Opener  AID : F0 46 4D 43 41 52 44 4D   (instantiated first)
 *   Applet  AID : F0 46 4D 43 41 52 44 01   (what hosts SELECT)
 *
 * There is no NFC tag applet and no hello applet on this card yet, so this
 * file names three AIDs and the whitelist holds three plus the package. A
 * Cashu-shaped AID here would be a card that does not exist.
 */
import {fromHex} from './bytes';

/** The card manager (Issuer Security Domain) of a J3R180. Never deleted. */
export const ISD_AID = fromHex('A000000151000000');

/** The card's package: F0 then "FMCARD". */
export const FEDIMINT_PACKAGE_AID = fromHex('F0464D43415244');
/**
 * The opener: instantiated first because the J3R180 will not take the card's
 * applet as its package's first instance.
 */
export const OPENER_AID = fromHex('F0464D434152444D');
/** The card's applet, selected by its whole AID. */
export const CARD_AID = fromHex('F0464D4341524401');

/**
 * Everything DELETE may name, and nothing else: the card's two instances and
 * its package. The ISD is not on it, and neither is anything this card does
 * not install.
 */
export const DELETABLE_AIDS: readonly (readonly number[])[] = [
  CARD_AID,
  OPENER_AID,
  FEDIMINT_PACKAGE_AID,
];
