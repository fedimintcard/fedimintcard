/**
 * The chip's identity, read from its CPLC (Card Production Life Cycle data,
 * GET DATA 9F7F on the card manager). Ops keys each card's card-manager key
 * set on it (docs/OPS-PLAN.md O5): the fields are written by the chip maker,
 * so they survive DELETE, a reinstall and a key rotation, where the card's
 * short id (its slot-0 key) does not.
 *
 * Ported from cashucard's tools/cardkit gp/identity.ts (the parse and the
 * read; see ../NOTICE). The fields are public: they name a chip, they do not
 * prove anyone owns it, and nothing here treats them as a secret.
 */
import {ISD_AID} from './aids';
import {type Bytes, bytes, toHex} from './bytes';
import {CardError, selectAid, transmit, type Transceiver} from './scp02';

export interface ChipIdentity {
  /** `cplc-v1:` and 24 hex: fabricator, IC type, fabrication date, serial, batch. */
  readonly id: string;
  readonly icFabricator: string;
  readonly icType: string;
  readonly icFabricationDate: string;
  readonly icSerialNumber: string;
  readonly icBatchIdentifier: string;
}

/** The form `ChipIdentity.id` takes; the registry checks the same pattern. */
export const CHIP_ID_PATTERN = /^cplc-v1:[0-9a-f]{24}$/;

/** GET DATA 9F7F, with Le: no key, no secure channel. */
export function getCplc(): number[] {
  return [0x80, 0xca, 0x9f, 0x7f, 0x00];
}

/** Exactly the 9F7F TLV of 42 data bytes, without its status word. */
export function parseCplc(data: Bytes): ChipIdentity {
  const b = bytes(data);
  if (b.length !== 45 || b[0] !== 0x9f || b[1] !== 0x7f || b[2] !== 42) {
    throw new RangeError('CPLC must be a 9F7F TLV with exactly 42 data bytes');
  }
  const serial = b.slice(15, 19);
  if (serial.every(v => v === 0) || serial.every(v => v === 0xff)) {
    // A chip with no serial cannot be told from another: its keys would be shared.
    throw new RangeError('CPLC serial is unset');
  }
  const hex = (start: number, end: number) =>
    toHex(b.slice(3 + start, 3 + end)).toLowerCase();
  const fields = {
    icFabricator: hex(0, 2),
    icType: hex(2, 4),
    icFabricationDate: hex(10, 12),
    icSerialNumber: hex(12, 16),
    icBatchIdentifier: hex(16, 18),
  };
  return {
    id: `cplc-v1:${fields.icFabricator}${fields.icType}${fields.icFabricationDate}${fields.icSerialNumber}${fields.icBatchIdentifier}`,
    ...fields,
  };
}

/**
 * SELECT of the card manager, then GET DATA 9F7F. No authentication, no
 * write, no retry. The applet is deselected afterwards, which ends any PIN
 * session it had open.
 */
export async function readChipIdentity(tx: Transceiver): Promise<ChipIdentity> {
  const selected = await transmit(tx, selectAid(ISD_AID));
  if (selected.sw !== 0x9000) {
    throw new CardError(selected.sw, 0xa4);
  }
  const reply = await transmit(tx, getCplc());
  if (reply.sw !== 0x9000) {
    throw new CardError(reply.sw, 0xca);
  }
  try {
    return parseCplc(reply.data);
  } catch (e) {
    throw new CardError(
      reply.sw,
      0xca,
      `the card's CPLC is not usable: ${e instanceof Error ? e.message : String(e)}`,
    );
  }
}
