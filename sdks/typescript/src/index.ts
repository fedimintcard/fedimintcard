import {
  CardProtocolError, CardStatusError, UnsupportedCardError, parseCertificate, parseSlot, parseState,
  publicKey, readResponse, type Transceive,
} from './protocol.js';

export {
  FEDIMINT_CARD_AID, CardProtocolError, CardStatusError, UnsupportedCardError, verifiesCertificate,
  type CardCertificate, type CardSlot, type CardState, type SlotState, type Transceive,
} from './protocol.js';

function atSlot(instruction: number, slot: number): number[] {
  if (!Number.isInteger(slot) || slot < 0 || slot >= 32) throw new RangeError('Slot must be 0 through 31');
  return [0xb0, instruction, slot, 0, 0];
}

/** A reader for a single physical connection. Create a new reader for every tap. */
export class FedimintCardReader {
  private constructor(
    private readonly send: Transceive,
    readonly version: Readonly<{ major: number; minor: number }>,
  ) {}

  static async connect(send: Transceive): Promise<FedimintCardReader> {
    const select = [0x00, 0xa4, 0x04, 0x00, 0x08, 0xf0, 0x46, 0x4d, 0x43, 0x41, 0x52, 0x44, 0x01, 0x00];
    const response = await readResponse(send, select, 2);
    if (response.length !== 2 || response[0] !== 4) throw new UnsupportedCardError();
    return new FedimintCardReader(send, Object.freeze({ major: response[0], minor: response[1] }));
  }

  async readState() {
    return parseState(await readResponse(this.send, [0xb0, 0x16, 0, 0, 0], 65));
  }

  async readSlot(slot: number) {
    return parseSlot(await readResponse(this.send, atSlot(0x15, slot), 187));
  }

  /** Empty slots return null. Other card errors retain their ISO status word. */
  async readPublicKey(slot = 0): Promise<Uint8Array | null> {
    try {
      return publicKey(await readResponse(this.send, atSlot(0x11, slot), 32));
    } catch (error) {
      if (error instanceof CardStatusError && error.statusWord === 0x6985) return null;
      throw error;
    }
  }

  /** Returns the unverified registry certificate, or null on an uncertified card. */
  async readCertificate() {
    try {
      return parseCertificate(await readResponse(this.send, [0xb0, 0x32, 0, 0, 0], 130));
    } catch (error) {
      if (error instanceof CardStatusError && error.statusWord === 0x6a83) return null;
      throw error;
    }
  }

  /** Inventory only. A federation or till must verify whether notes can settle. */
  async readBalance(): Promise<{ notesMsat: bigint; pendingChangeMsat: bigint }> {
    const { slots } = await this.readState();
    let notesMsat = 0n;
    let pendingChangeMsat = 0n;
    for (const [index, state] of slots.entries()) {
      if (state !== 'note' && state !== 'change') continue;
      const slot = await this.readSlot(index);
      if (slot.state !== state) throw new CardProtocolError('Slot state changed during balance read');
      if (state === 'note') notesMsat += slot.amountMsat;
      else pendingChangeMsat += slot.amountMsat;
    }
    return { notesMsat, pendingChangeMsat };
  }
}

export * from './till.js';
