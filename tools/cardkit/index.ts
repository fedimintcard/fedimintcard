/**
 * cardkit: GlobalPlatform over an injected transceiver: SCP02, the card-manager
 * commands and a recovering install, for the phone-side provisioner
 * (docs/PROVISIONER-PLAN.md P1a). Environment-neutral core: no NFC, no HTTP,
 * no platform import. The transport is injected as `Transceiver`, so the phone
 * and Node run the same code, and gp/test runs it against a fake card.
 */
export * from './gp/aids';
export {concat, equal, fromHex, swHex, toHex} from './gp/bytes';
export type {Bytes} from './gp/bytes';
export {
  DES,
  desEcb,
  pad80,
  retailMac,
  tdesCbc,
  tdesCbcMac,
  tdesEcb,
} from './gp/des';
export * from './gp/scp02';
export * from './gp/provision';
