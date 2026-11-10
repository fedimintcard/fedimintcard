/**
 * A card back to its factory state (OPS-PLAN O5, after cashucard's owned-card
 * reset): this card's package gone, the card manager back on the chip maker's
 * keys, and both proved by a login under those keys. Never installs, never
 * retries, and names nothing on the card but the Fedimint AIDs.
 *
 * One run is level-triggered, like the install: it reads where the card is
 * and goes on from there, so a tap lifted at any point is finished by the
 * next. One INITIALIZE UPDATE, whose card cryptogram is checked offline
 * against the card's own set and the factory set; EXTERNAL AUTHENTICATE goes
 * out only for the set the card proved (a failed login counts towards a lock
 * that is for good). Then GET STATUS says whether any of this card's objects
 * are there:
 *
 *   own keys,     ours present → DELETE (applet, opener, package) → PUT KEY → prove
 *   own keys,     ours absent  → PUT KEY → prove
 *   factory keys, ours absent  → done: this login and this GET STATUS are the proof
 *   factory keys, ours present → stop, `guarded`: not a card this app moved to its own keys
 *
 * PUT KEY replaces the set in place: P1 the version the card reported, and
 * the new set's version `FACTORY_KEY_VERSION`, which is what a factory J3R180
 * reports (01). A card whose set is any other version stops before anything
 * is written. The proof is a fresh SELECT, INITIALIZE UPDATE that must prove
 * the factory set at version 01, EXTERNAL AUTHENTICATE, and GET STATUS that
 * lists none of ours.
 *
 * `beforeWrite` runs once, just before the first DELETE or PUT KEY of the
 * run, so the caller can write its journal; whatever it throws ends the run
 * with nothing written.
 *
 * A factory set is every blank card's: the caller must have matched the
 * chip's CPLC in this same tap before it trusts a `factory` result.
 */
import {CARD_AID, FEDIMINT_PACKAGE_AID, ISD_AID, OPENER_AID} from './aids';
import {type Bytes, bytes, toHex} from './bytes';
import {
  Guard,
  Run,
  expectOk,
  type RunOptions,
  type RunResult,
} from './provision';
import {
  CardError,
  FACTORY_KEYS,
  type KeySet,
  type Reply,
  type SecureSession,
  type Transceiver,
  WrongKeys,
  deleteAid,
  externalAuthenticate,
  initializeUpdate,
  putKeys,
  selectAid,
  sessionIfMatches,
} from './scp02';

/**
 * The key set version a factory J3R180 carries, and the one a reset restores.
 * The owner's `gp -i -v` of a spare lists ENC/MAC/DEK as version 1 (0x01),
 * and `gp --unlock` back to the factory keys sends PUT KEY 84 D8 01 81 with
 * new version 01 (cashucard's gp-info.txt and gp-unlock-factory.txt).
 */
export const FACTORY_KEY_VERSION = 0x01;

export type FactoryResetPhase =
  'selectIsd' | 'auth' | 'status' | 'erase' | 'restore' | 'prove' | 'done';

/** Where the run found the card: which set it proved, and whether any of ours was there. */
export type FoundState = 'own-present' | 'own-absent' | 'factory-absent';

export interface FactoryResetOptions extends RunOptions {
  /** Called once, before the first DELETE or PUT KEY; a throw ends the run with nothing written. */
  beforeWrite?: () => Promise<void>;
}

export interface FactoryResetResult extends RunResult<FactoryResetPhase> {
  /** What the run found after its login, once it got that far. */
  found?: FoundState;
  /** The factory set's version, from the login that proved it; set when `ok`. */
  keyVersion?: number;
}

/** Which of this card's objects GET STATUS lists. */
export interface Ours {
  readonly card: boolean;
  readonly opener: boolean;
  readonly package: boolean;
}

export const anyOurs = (o: Ours): boolean => o.card || o.opener || o.package;

/** GET STATUS, TLV form (P2 02, then 03 for the next page), every entry (4F 00). */
export function getStatusCommand(
  kind: 'applications' | 'loadFiles',
  next = false,
): number[] {
  return [
    0x80,
    0xf2,
    kind === 'applications' ? 0x40 : 0x20,
    next ? 0x03 : 0x02,
    0x02,
    0x4f,
    0x00,
    0x00,
  ];
}

/** GET STATUS answered something this module will not read as present or absent. */
class Unreadable extends Guard {
  constructor(why: string) {
    super(`GET STATUS: ${why}; nothing more was sent`);
  }
}

interface Tlv {
  tag: number;
  value: number[];
}

/** One level of BER-TLV, definite lengths up to two bytes. */
function tlvs(data: Bytes): Tlv[] {
  const b = bytes(data);
  const out: Tlv[] = [];
  let at = 0;
  while (at < b.length) {
    let tag = b[at++];
    if ((tag & 0x1f) === 0x1f) {
      const second = b[at++];
      if (second === undefined || (second & 0x80) !== 0) {
        throw new Unreadable('a tag the parser does not take');
      }
      tag = (tag << 8) | second;
    }
    let length = b[at++];
    if (length === undefined) {
      throw new Unreadable('a TLV with no length');
    }
    if (length === 0x81) {
      length = b[at++];
    } else if (length === 0x82) {
      length = (b[at++] << 8) | b[at++];
    } else if (length > 0x80) {
      throw new Unreadable('a length the parser does not take');
    }
    if (
      length === undefined ||
      Number.isNaN(length) ||
      at + length > b.length
    ) {
      throw new Unreadable('a TLV runs past the reply');
    }
    out.push({tag, value: b.slice(at, at + length)});
    at += length;
  }
  return out;
}

/** The AID (4F) and the load file (C4) of each E3 entry. */
export function parseStatusEntries(
  data: Bytes,
): {aid: string; loadFile: string | null}[] {
  return tlvs(data).map(entry => {
    if (entry.tag !== 0xe3) {
      throw new Unreadable(`an entry tagged ${entry.tag.toString(16)}, not E3`);
    }
    const fields = tlvs(entry.value);
    const aids = fields.filter(f => f.tag === 0x4f);
    if (aids.length !== 1) {
      throw new Unreadable('an entry without exactly one AID');
    }
    const lf = fields.find(f => f.tag === 0xc4);
    return {aid: toHex(aids[0].value), loadFile: lf ? toHex(lf.value) : null};
  });
}

/** Every entry of one GET STATUS, across its pages. 6A88 on the first page is none. */
async function statusOf(
  send: (apdu: number[]) => Promise<Reply>,
  kind: 'applications' | 'loadFiles',
): Promise<{aid: string; loadFile: string | null}[]> {
  const data: number[] = [];
  for (let page = 0; page < 16; page++) {
    const reply = await send(getStatusCommand(kind, page > 0));
    if (page === 0 && reply.sw === 0x6a88) {
      return [];
    }
    if (reply.sw !== 0x9000 && reply.sw !== 0x6310) {
      throw new CardError(reply.sw, 0xf2);
    }
    data.push(...reply.data);
    if (reply.sw === 0x9000) {
      return parseStatusEntries(data);
    }
  }
  throw new Unreadable('more than 16 pages');
}

/**
 * Which of this card's objects the card lists. Anything made from our package
 * counts, whatever its AID; nothing else on the card is looked at twice.
 */
export async function readOurs(
  send: (apdu: number[]) => Promise<Reply>,
): Promise<Ours> {
  const pkg = toHex(FEDIMINT_PACKAGE_AID);
  const card = toHex(CARD_AID);
  const opener = toHex(OPENER_AID);
  const apps = await statusOf(send, 'applications');
  const files = await statusOf(send, 'loadFiles');
  const stray = apps.find(
    a => a.loadFile === pkg && a.aid !== card && a.aid !== opener,
  );
  if (stray) {
    throw new Guard(
      `${stray.aid} is an instance of the Fedimint package this app did not make: nothing was deleted`,
    );
  }
  return {
    card: apps.some(a => a.aid === card),
    opener: apps.some(a => a.aid === opener),
    package: files.some(f => f.aid === pkg),
  };
}

/** DELETE, in this order: the instances first (a package with instances refuses), then the package. */
const ERASE: readonly (readonly number[])[] = [
  CARD_AID,
  OPENER_AID,
  FEDIMINT_PACKAGE_AID,
];

/**
 * Takes the card to factory: see the header. `own` is the card's own set from
 * ops. Returns how the run ended; throws only for the caller's fault (a bug)
 * or for what `beforeWrite` throws that is not the card's.
 */
export async function factoryReset(
  tx: Transceiver,
  own: KeySet,
  options: FactoryResetOptions,
): Promise<FactoryResetResult> {
  const run = new Run<FactoryResetPhase>(tx, options, 'selectIsd');
  const sessionOptions = {macOverModifiedApdu: options.macOverModifiedApdu};
  let found: FoundState | undefined;
  let wrote = false;
  const beforeWrite = async () => {
    if (!wrote) {
      wrote = true;
      await options.beforeWrite?.();
    }
  };
  try {
    run.enter('selectIsd');
    expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);

    run.enter('auth');
    const hs = await initializeUpdate(run.timed, options.random, 0);
    const ownKeys = sessionIfMatches(own, hs);
    const factoryKeys = ownKeys ? null : sessionIfMatches(FACTORY_KEYS, hs);
    if (!ownKeys && !factoryKeys) {
      throw new WrongKeys(
        'the card proved neither its own keys nor the factory keys; EXTERNAL AUTHENTICATE was not sent',
      );
    }
    const kvn = hs.reply.kvn;
    if (kvn !== FACTORY_KEY_VERSION) {
      // PUT KEY in place would keep this version, and the card would not be factory.
      throw new Guard(
        `the card's key set is version ${kvn}, not ${FACTORY_KEY_VERSION}: nothing was written`,
      );
    }
    let session: SecureSession = await externalAuthenticate(
      run.timed,
      (ownKeys ?? factoryKeys)!,
      hs,
      sessionOptions,
    );
    const send = (apdu: number[]) => session.send(run.timed, apdu);

    run.enter('status');
    let ours = await readOurs(send);
    const present = anyOurs(ours);
    found = ownKeys
      ? present
        ? 'own-present'
        : 'own-absent'
      : present
        ? undefined
        : 'factory-absent';
    if (!found) {
      throw new Guard(
        'the card is on the factory keys and still has the Fedimint applet: it is not a card this app moved to its own keys, so nothing was deleted',
      );
    }
    if (found === 'factory-absent') {
      run.enter('done');
      return {...run.finish({ok: true}), found, keyVersion: kvn};
    }

    if (found === 'own-present') {
      run.enter('erase');
      await beforeWrite();
      for (const aid of ERASE) {
        const r = await send(deleteAid(aid));
        // 6A88: already gone, by a DELETE whose reply a lifted tap lost.
        if (r.sw !== 0x9000 && r.sw !== 0x6a88) {
          throw new CardError(r.sw, 0xe4);
        }
      }
      ours = await readOurs(send);
      if (anyOurs(ours)) {
        throw new Guard(
          'the card still lists the Fedimint package after DELETE: the keys were not restored',
        );
      }
    }

    run.enter('restore');
    await beforeWrite();
    expectOk(
      await send(
        putKeys(FACTORY_KEYS, session, {
          p1: kvn,
          newKvn: FACTORY_KEY_VERSION,
        }),
      ),
      0xd8,
    );

    run.enter('prove');
    expectOk(await run.plain(selectAid(ISD_AID)), 0xa4);
    const again = await initializeUpdate(run.timed, options.random, 0);
    const proved = sessionIfMatches(FACTORY_KEYS, again);
    if (!proved) {
      throw new WrongKeys(
        sessionIfMatches(own, again)
          ? 'the card is still on its own keys after PUT KEY; EXTERNAL AUTHENTICATE under the factory keys was not sent'
          : 'after PUT KEY the card proved neither key set; EXTERNAL AUTHENTICATE was not sent',
      );
    }
    if (again.reply.kvn !== FACTORY_KEY_VERSION) {
      throw new Guard(
        `the factory keys came back as version ${again.reply.kvn}, not ${FACTORY_KEY_VERSION}`,
      );
    }
    session = await externalAuthenticate(
      run.timed,
      proved,
      again,
      sessionOptions,
    );
    if (anyOurs(await readOurs(send))) {
      throw new Guard('the card lists the Fedimint package after the reset');
    }
    run.enter('done');
    return {
      ...run.finish({ok: true}),
      found,
      keyVersion: again.reply.kvn,
    };
  } catch (e) {
    return {...run.fail(e), found};
  }
}
