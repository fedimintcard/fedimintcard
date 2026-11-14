/**
 * Offline card payments, the registry's half (docs/OFFLINE-PROTOCOL.md § 2-3, 6-7).
 *
 * What lives here: the certificate that vouches for a card's attestation key
 * (signed with `CERT_SIGNING_KEY`), the route a till uses to report a charge
 * the card signed with no connection, the report log an operator reads, the
 * releases (§ 6), and the slots a till found stranded by a sale the registry
 * never saw, held and then released (§ 7).
 *
 * The registry cannot stop an offline double spend; the federation's spent set
 * does. What it does is decide, once the till is back online, whether the
 * card's inputs were still live. If so the charge enters the normal
 * intent/submitted/outcome flow already open and signed; if not it is refused
 * with the conflicting notes and kept as a report.
 */
import {
  bump,
  cardOwner,
  MAX_TX_HEX,
  openStrandings,
  parseIntent,
  slotCount,
  STRANDED_GRACE_MS,
  txKnown,
  versionOf,
  type ChangeOut,
} from './cards';
import {
  BadRequest,
  Conflict,
  fromHex,
  isHex,
  isPositiveInt,
  NotFound,
  ServiceUnavailable,
  toHex,
} from './http';
import {keyUsed} from './loads';

type Sql = SqlStorage;

const P256 = {name: 'ECDSA', namedCurve: 'P-256'} as const;
/** Domain separator of the certificate signature (OFFLINE-PROTOCOL § 2), 18 ASCII bytes. */
const CERT_DOMAIN = new TextEncoder().encode('fedimintcard/cert1');
const REPORTS_LIMIT = 200;

/* ------------------------------------------------------------------------- */
/* Certificate                                                               */
/* ------------------------------------------------------------------------- */

async function certKey(secret: string | undefined): Promise<CryptoKey> {
  if (!secret) {
    throw new ServiceUnavailable('CERT_SIGNING_KEY is not set; the registry cannot issue certificates');
  }
  try {
    // Extractable so the public point can be derived from the same import.
    return await crypto.subtle.importKey('pkcs8', fromBase64(secret), P256, true, ['sign']);
  } catch {
    throw new ServiceUnavailable('CERT_SIGNING_KEY is not a P-256 private key, PKCS#8 DER, base64');
  }
}

function fromBase64(text: string): Uint8Array {
  const bin = atob(text.trim());
  return Uint8Array.from(bin, c => c.charCodeAt(0));
}

function fromBase64Url(text: string): Uint8Array {
  return fromBase64(text.replace(/-/g, '+').replace(/_/g, '/'));
}

/** The certificate key's public point, 65 bytes uncompressed, hex (`GET /registry/cert-key`). */
export async function certPublicKey(secret: string | undefined): Promise<string> {
  const jwk = (await crypto.subtle.exportKey('jwk', await certKey(secret))) as JsonWebKey;
  return toHex(new Uint8Array([4, ...fromBase64Url(jwk.x!), ...fromBase64Url(jwk.y!)]));
}

/** A valid uncompressed P-256 point, hex; WebCrypto rejects an off-curve one. */
async function parseAttestKey(value: unknown): Promise<Uint8Array> {
  if (!isHex(value, 65) || !value.startsWith('04')) {
    throw new BadRequest('attest_pub must be an uncompressed P-256 point, 65 bytes hex starting 04');
  }
  const raw = fromHex(value);
  try {
    await crypto.subtle.importKey('raw', raw, P256, true, ['verify']);
  } catch {
    throw new BadRequest('attest_pub is not a point on P-256');
  }
  return raw;
}

/** `01 ‖ slot 0 x-only ‖ compressed attestation key ‖ r‖s` (130 bytes, hex). */
async function buildCert(secret: string | undefined, slot0: string, attest: Uint8Array): Promise<string> {
  const key = await certKey(secret);
  const compressed = new Uint8Array(33);
  compressed[0] = attest[64]! % 2 === 0 ? 2 : 3;
  compressed.set(attest.subarray(1, 33), 1);
  const x = fromHex(slot0);
  const message = new Uint8Array([...CERT_DOMAIN, ...x, ...compressed]);
  // WebCrypto's ECDSA output is already raw r ‖ s.
  const sig = new Uint8Array(await crypto.subtle.sign({name: 'ECDSA', hash: 'SHA-256'}, key, message));
  return toHex(new Uint8Array([1, ...x, ...compressed, ...sig]));
}

/**
 * Stores a card's attestation key and answers its certificate. The same key
 * again answers the stored certificate (ECDSA signatures are randomized, so
 * re-signing would give different bytes); a different key replaces it only
 * while the card has no live or pending note, since notes already issued to a
 * till may rely on the old certificate.
 */
export async function recordAttestation(
  storage: DurableObjectStorage,
  secret: string | undefined,
  operatorId: number,
  shortId: string,
  r: Record<string, unknown>,
): Promise<{cert: string}> {
  const sql = storage.sql;
  const attest = await parseAttestKey(r.attest_pub);
  const attestHex = toHex(attest);
  const named = r.slot0 === undefined ? null : r.slot0;
  if (named !== null && (!isHex(named, 32) || named !== named.toLowerCase())) {
    throw new BadRequest('slot0 must be slot 0\u2019s x-only key, 32 bytes lower-case hex');
  }
  const exists = sql.exec('SELECT 1 FROM cards WHERE short_id = ?', shortId).toArray().length > 0;
  if (!exists) {
    // The install tap asks before the app registers the card: certify the key the card
    // names, held until the card is stored with that slot 0 (`adoptPendingAttestation`).
    if (named === null) throw new NotFound(`no card ${shortId}`);
    return await recordPendingAttestation(storage, secret, operatorId, shortId, named, attest, attestHex, Date.now());
  }
  if (cardOwner(sql, shortId) !== operatorId) {
    throw new NotFound(`no card ${shortId}`);
  }
  const read = () => {
    const slot0 = sql.exec('SELECT pubkey FROM card_slots WHERE short_id = ? AND slot = 0', shortId).toArray()[0];
    if (!slot0) throw new Conflict(`card ${shortId} has no slot 0 key on record`);
    if (named !== null && String(slot0.pubkey) !== named) {
      throw new Conflict(`card ${shortId}'s slot 0 on record is not the key named`);
    }
    const row = sql.exec('SELECT attest_pub, attest_cert FROM cards WHERE short_id = ?', shortId).one();
    return {slot0: String(slot0.pubkey), pub: row.attest_pub as string | null, cert: row.attest_cert as string | null};
  };
  const check = (cur: ReturnType<typeof read>): string | null => {
    if (cur.pub === attestHex && cur.cert) return cur.cert;
    if (cur.pub !== null && cur.pub !== attestHex) {
      const held = sql
        .exec("SELECT COUNT(*) AS n FROM notes WHERE short_id = ? AND status IN ('live', 'pending')", shortId)
        .one();
      if (Number(held.n) > 0) {
        throw new Conflict(`card ${shortId} has another attestation key and holds live or pending notes`);
      }
    }
    return null;
  };
  const first = read();
  const stored = check(first);
  if (stored) return {cert: stored};
  const cert = await buildCert(secret, first.slot0, attest);
  // Signing awaited: decide again on the rows as they are now.
  return storage.transactionSync(() => {
    const cur = read();
    const again = check(cur);
    if (again) return {cert: again};
    sql.exec('UPDATE cards SET attest_pub = ?, attest_cert = ? WHERE short_id = ?', attestHex, cert, shortId);
    return {cert};
  });
}

/** Pending certificates older than this are dropped: an install tap that never registered its card. */
const PENDING_TTL_MS = 7 * 24 * 60 * 60 * 1000;

async function recordPendingAttestation(
  storage: DurableObjectStorage,
  secret: string | undefined,
  operatorId: number,
  shortId: string,
  slot0: string,
  attest: Uint8Array,
  attestHex: string,
  now: number,
): Promise<{cert: string}> {
  const sql = storage.sql;
  if (slot0.slice(-8) !== shortId) {
    throw new BadRequest(`slot0 is not card ${shortId}'s key (a short id is its last 8 hex digits)`);
  }
  const held = () => {
    sql.exec('DELETE FROM pending_attestations WHERE created_at < ?', now - PENDING_TTL_MS);
    const row = sql.exec('SELECT * FROM pending_attestations WHERE short_id = ?', shortId).toArray()[0];
    if (!row) return null;
    if (Number(row.operator_id) !== operatorId) throw new Conflict(`card ${shortId} is being set up by another operator`);
    return row.slot0 === slot0 && row.attest_pub === attestHex ? String(row.attest_cert) : null;
  };
  const first = held();
  if (first) return {cert: first};
  const cert = await buildCert(secret, slot0, attest);
  return storage.transactionSync(() => {
    const again = held();
    if (again) return {cert: again};
    sql.exec(
      `INSERT OR REPLACE INTO pending_attestations (short_id, operator_id, slot0, attest_pub, attest_cert, created_at)
       VALUES (?, ?, ?, ?, ?, ?)`,
      shortId, operatorId, slot0, attestHex, cert, now,
    );
    return {cert};
  });
}

/**
 * Inside `insertCard`'s transaction: a certificate the install tap asked for moves onto the new
 * card row when it was this operator's and its slot 0 is the card's. Any other pending row for
 * the short id is dropped: it certified a key the stored card does not have.
 */
export function adoptPendingAttestation(sql: SqlStorage, operatorId: number, shortId: string, slot0: string | undefined): void {
  const row = sql.exec('SELECT * FROM pending_attestations WHERE short_id = ?', shortId).toArray()[0];
  if (!row) return;
  sql.exec('DELETE FROM pending_attestations WHERE short_id = ?', shortId);
  if (Number(row.operator_id) === operatorId && slot0 !== undefined && row.slot0 === slot0) {
    sql.exec('UPDATE cards SET attest_pub = ?, attest_cert = ? WHERE short_id = ?', row.attest_pub, row.attest_cert, shortId);
  }
}

/* ------------------------------------------------------------------------- */
/* Offline charges                                                           */
/* ------------------------------------------------------------------------- */

export interface Conflicts {
  readonly nonce: string;
  readonly status: string;
  readonly txid: string | null;
}

export type OfflineResult =
  | {recorded: true; version: number; replayed: boolean}
  | {recorded: false; conflicts: Conflicts[]; release: {txid: string; slots: number[]} | null};

/**
 * An offline charge reported after the fact. Unlike `recordIntent` it takes no
 * card version and ignores another till's open charge: the card already signed,
 * so the only question is whether every input is still live. The slot checks of
 * `recordIntent` are not repeated either (the card chose its change slots
 * offline); only a change nonce that already carried a note is refused, because
 * it would overwrite a note row.
 *
 * Idempotency, by txid: a charge already in `card_txs` answers the current
 * version; a double spend already reported answers its stored conflicts.
 *
 * A transaction the registry released a stranded slot for (§ 7) is refused as
 * a double spend whose conflicts name every input `released`, whatever the
 * notes' state: it will never be accepted.
 */
export function recordOfflineCharge(
  storage: DurableObjectStorage,
  shortId: string,
  tillId: number,
  r: Record<string, unknown>,
  now: number,
): OfflineResult {
  const sql = storage.sql;
  const intent = parseIntent(r, slotCount(sql, shortId));
  if (intent.body.inputs.length === 0 || intent.body.tx_hex === null) {
    throw new BadRequest('an offline charge spends at least one note and carries its unsigned tx_hex');
  }
  const signed = r.signed_hex;
  if (
    typeof signed !== 'string' ||
    signed.length === 0 ||
    signed.length > MAX_TX_HEX ||
    signed.length % 2 !== 0 ||
    !/^[0-9a-f]*$/.test(signed)
  ) {
    throw new BadRequest('signed_hex must be the signed transaction, lower-case hex');
  }
  if (!isPositiveInt(r.signed_at)) {
    throw new BadRequest('signed_at must be the card’s signing time, a positive integer in ms');
  }
  return storage.transactionSync((): OfflineResult => {
    const prior = sql.exec('SELECT short_id FROM card_txs WHERE txid = ?', intent.txid).toArray()[0];
    if (prior) {
      if (prior.short_id !== shortId) throw new Conflict(`transaction ${intent.txid} belongs to another card`);
      return {recorded: true, version: versionOf(sql, shortId), replayed: true};
    }
    const reported = sql
      .exec("SELECT detail FROM offline_reports WHERE txid = ? AND short_id = ? AND result = 'double_spend' LIMIT 1", intent.txid, shortId)
      .toArray()[0];
    if (reported) {
      const stored = (JSON.parse(String(reported.detail)) as {conflicts: Conflicts[]}).conflicts;
      return {recorded: false, conflicts: stored, release: null};
    }
    const report = (result: 'recorded' | 'double_spend', detail: unknown) =>
      sql.exec(
        'INSERT INTO offline_reports (txid, short_id, till_id, result, detail, created_at) VALUES (?, ?, ?, ?, ?, ?)',
        intent.txid, shortId, tillId, result, JSON.stringify(detail), now,
      );
    // The money gate (§ 7): a release is the registry's word that this transaction will never be
    // accepted, and the card may already pay with the released note again. Here a release can only
    // be a stranding's (a charge on record or a refused report answered above, and neither row is
    // ever deleted), so the gate rests on `releases` alone, which the backup keeps. Its report's
    // conflicts are all `released`, which `txKnown` reads: the transaction's other held slots keep
    // to their own release.
    if (hasRelease(sql, shortId, intent.txid)) {
      const refused: Conflicts[] = intent.body.inputs.map(nonce => ({nonce, status: 'released', txid: intent.txid}));
      report('double_spend', {client_ref: intent.client_ref, signed_at: r.signed_at, inputs: intent.body.inputs, conflicts: refused});
      // Its slots are released already: nothing new to sign.
      return {recorded: false, conflicts: refused, release: null};
    }
    const conflicts: Conflicts[] = [];
    for (const nonce of intent.body.inputs) {
      const note = sql
        .exec('SELECT status, txid FROM notes WHERE short_id = ? AND nonce = ?', shortId, nonce)
        .toArray()[0];
      if (!note) conflicts.push({nonce, status: 'missing', txid: null});
      else if (note.status !== 'live') {
        conflicts.push({nonce, status: String(note.status), txid: note.txid === null ? null : String(note.txid)});
      }
    }
    if (conflicts.length > 0) {
      report('double_spend', {client_ref: intent.client_ref, signed_at: r.signed_at, inputs: intent.body.inputs, conflicts});
      // The card's change slots wait for change that will never come; its inputs were spent
      // elsewhere and stay spent (docs/OFFLINE-PROTOCOL.md § 6).
      const slots = intent.body.change.map(c => c.slot).sort((a, b) => a - b);
      return {recorded: false, conflicts, release: slots.length > 0 ? {txid: intent.txid, slots} : null};
    }
    for (const c of intent.body.change) {
      const used = sql.exec('SELECT 1 FROM notes WHERE short_id = ? AND nonce = ?', shortId, c.nonce).toArray();
      if (used.length > 0) {
        throw new Conflict(`change nonce ${c.nonce} already carried a note on card ${shortId}`);
      }
    }
    sql.exec(
      `INSERT INTO card_txs (txid, short_id, till_id, client_ref, body, status, outcome, created_at, resolved_at, signed_hex)
       VALUES (?, ?, ?, ?, ?, 'open', NULL, ?, NULL, ?)`,
      intent.txid, shortId, tillId, intent.client_ref, JSON.stringify(intent.body), now, signed,
    );
    for (const nonce of intent.body.inputs) {
      sql.exec("UPDATE notes SET status = 'pending', txid = ? WHERE short_id = ? AND nonce = ?", intent.txid, shortId, nonce);
    }
    for (const c of intent.body.change as ChangeOut[]) {
      sql.exec(
        `INSERT INTO notes (short_id, nonce, slot, amount_msat, signature, blinding_key, status, txid, out_idx)
         VALUES (?, ?, ?, ?, NULL, ?, 'pending', ?, ?)`,
        shortId, c.nonce, c.slot, c.amount_msat, c.blinding_key, intent.txid, c.out_idx,
      );
    }
    report('recorded', {client_ref: intent.client_ref, signed_at: r.signed_at, inputs: intent.body.inputs});
    return {recorded: true, version: bump(sql, shortId), replayed: false};
  });
}

/* ------------------------------------------------------------------------- */
/* Releases (§ 6)                                                            */
/* ------------------------------------------------------------------------- */

/** Domain separator of a release, 17 ASCII bytes. */
const RELEASE_DOMAIN = new TextEncoder().encode('fedimintcard/rel1');

/** One INTEGER of a DER signature: minimal, positive (a leading 00 only before a high bit). */
function derInteger(bytes: Uint8Array): Uint8Array {
  let i = 0;
  while (i < bytes.length - 1 && bytes[i] === 0) i += 1;
  const v = bytes.subarray(i);
  const body = v[0]! & 0x80 ? new Uint8Array([0, ...v]) : v;
  return new Uint8Array([0x02, body.length, ...body]);
}

/** WebCrypto's ECDSA P-256 signature, raw r ‖ s (64 bytes), as DER, which the card verifies. */
export function rawToDer(raw: Uint8Array): Uint8Array {
  if (raw.length !== 64) throw new Error('a P-256 signature is 64 bytes, r ‖ s');
  const r = derInteger(raw.subarray(0, 32));
  const s = derInteger(raw.subarray(32));
  return new Uint8Array([0x30, r.length + s.length, ...r, ...s]);
}

/** A DER ECDSA P-256 signature as raw r ‖ s (64 bytes); throws on anything else. */
export function derToRaw(der: Uint8Array): Uint8Array {
  const fail = () => {
    throw new Error('not a DER P-256 signature');
  };
  if (der.length < 8 || der[0] !== 0x30 || der[1] !== der.length - 2) fail();
  const out = new Uint8Array(64);
  let at = 2;
  for (const half of [0, 32]) {
    if (der[at] !== 0x02) fail();
    const len = der[at + 1]!;
    const v = der.subarray(at + 2, at + 2 + len);
    if (len < 1 || v.length !== len || v[0]! & 0x80) fail();
    if (len > 1 && v[0] === 0 && !(v[1]! & 0x80)) fail();
    const trimmed = v[0] === 0 && len > 1 ? v.subarray(1) : v;
    if (trimmed.length > 32) fail();
    out.set(trimmed, half + 32 - trimmed.length);
    at += 2 + len;
  }
  if (at !== der.length) fail();
  return out;
}

/** What a release signs: the label, slot 0's x-only key, the slot, the txid. */
export function releaseMessage(slot0: string, slot: number, txid: string): Uint8Array {
  return new Uint8Array([...RELEASE_DOMAIN, ...fromHex(slot0), slot, ...fromHex(txid)]);
}

/**
 * Sign and keep a release for each of `slots` of card `shortId` for `txid`, a transaction the
 * registry has just recorded as never to be accepted. Signing awaits, so it happens outside the
 * transaction that recorded the refusal; the rows are written after, in a transaction that checks
 * `still` (the refusal is on record) and that slot 0 is the key that was signed over.
 *
 * Never throws: with no `CERT_SIGNING_KEY` (or any other failure) it logs and records nothing,
 * and the outcome or report it follows stands.
 */
export async function recordReleases(
  storage: DurableObjectStorage,
  secret: string | undefined,
  shortId: string,
  txid: string,
  slots: readonly number[],
  still: (sql: SqlStorage) => boolean,
  now: number,
): Promise<number> {
  if (slots.length === 0) return 0;
  const sql = storage.sql;
  try {
    if (!secret) {
      console.warn(`releases: CERT_SIGNING_KEY is not set; none signed for ${txid} on card ${shortId} (slots ${slots.join(', ')})`);
      return 0;
    }
    const slot0Of = () => sql.exec('SELECT pubkey FROM card_slots WHERE short_id = ? AND slot = 0', shortId).toArray()[0]?.pubkey;
    const slot0 = slot0Of();
    if (typeof slot0 !== 'string') {
      console.warn(`releases: card ${shortId} has no slot 0 key on record; none signed for ${txid}`);
      return 0;
    }
    const key = await certKey(secret);
    const signed: {slot: number; sig: string}[] = [];
    for (const slot of slots) {
      const raw = new Uint8Array(
        await crypto.subtle.sign({name: 'ECDSA', hash: 'SHA-256'}, key, releaseMessage(slot0, slot, txid)),
      );
      signed.push({slot, sig: toHex(rawToDer(raw))});
    }
    return storage.transactionSync(() => {
      if (!still(sql) || slot0Of() !== slot0) return 0;
      for (const {slot, sig} of signed) {
        sql.exec(
          'INSERT OR IGNORE INTO releases (short_id, slot, txid, sig, created_at) VALUES (?, ?, ?, ?, ?)',
          shortId, slot, txid, sig, now,
        );
      }
      return signed.length;
    });
  } catch (e) {
    console.warn(`releases: none recorded for ${txid} on card ${shortId}: ${e instanceof Error ? e.message : String(e)}`);
    return 0;
  }
}

/** Still true when the releases for a rejected charge are written. */
export const chargeRejected = (txid: string) => (sql: SqlStorage) =>
  sql.exec("SELECT 1 FROM card_txs WHERE txid = ? AND status = 'rejected'", txid).toArray().length > 0;

/** Still true when the releases for an offline report refused as a double spend are written. */
export const reportRefused = (txid: string, shortId: string) => (sql: SqlStorage) =>
  sql
    .exec("SELECT 1 FROM offline_reports WHERE txid = ? AND short_id = ? AND result = 'double_spend'", txid, shortId)
    .toArray().length > 0;

/* ------------------------------------------------------------------------- */
/* Strandings (§ 7)                                                          */
/* ------------------------------------------------------------------------- */

/** Most slots one report names: every slot of the largest card. */
const MAX_STRANDED = 32;

/** A slot a till found SPENT or CHANGE for a transaction it could not account for. */
interface StrandedSlot {
  readonly slot: number;
  readonly kind: 'spent' | 'change';
  /** The slot's spent hash (SPENT) or its change opening's txid (CHANGE), hex. */
  readonly txid: string;
  /** A spent slot only: the federation answered that its note is unspent. */
  readonly unspent: boolean;
  /** A spent slot only: the federation answered that its note is spent. */
  readonly spent: boolean;
}

/** `{slots: [{slot, kind, txid, unspent?, spent?}]}`, 1 to 32 slots, each named once. */
function parseStranded(r: Record<string, unknown>, slots: number): StrandedSlot[] {
  if (!Array.isArray(r.slots) || r.slots.length === 0 || r.slots.length > MAX_STRANDED) {
    throw new BadRequest(`slots must list 1 to ${MAX_STRANDED} {slot, kind, txid}`);
  }
  // A flag is a boolean when sent; null reads as not sent.
  const isFlag = (v: unknown) => v === undefined || v === null || typeof v === 'boolean';
  const out = r.slots.map((e): StrandedSlot => {
    const o = e as Record<string, unknown>;
    if (
      typeof o !== 'object' ||
      o === null ||
      !Number.isSafeInteger(o.slot) ||
      (o.slot as number) < 0 ||
      (o.slot as number) >= slots ||
      (o.kind !== 'spent' && o.kind !== 'change') ||
      !isHex(o.txid, 32) ||
      !isFlag(o.unspent) ||
      !isFlag(o.spent)
    ) {
      throw new BadRequest(
        `each slot needs slot 0-${slots - 1}, kind 'spent' or 'change', txid (32 bytes hex), and unspent or spent only as booleans`,
      );
    }
    if (o.unspent === true && o.spent === true) {
      throw new BadRequest(`slot ${o.slot as number}: a note is not both spent and unspent`);
    }
    // The federation's answer is about a note: a change slot has none, so the flags mean nothing there.
    const note = o.kind === 'spent';
    return {slot: o.slot as number, kind: o.kind, txid: o.txid, unspent: note && o.unspent === true, spent: note && o.spent === true};
  });
  if (new Set(out.map(s => s.slot)).size !== out.length) {
    throw new BadRequest('slots name a slot twice');
  }
  return out;
}

/** Any release of the card for the transaction. */
function hasRelease(sql: Sql, shortId: string, txid: string): boolean {
  return sql.exec('SELECT 1 FROM releases WHERE short_id = ? AND txid = ?', shortId, txid).toArray().length > 0;
}

/** The release of one slot for the transaction. */
function slotReleased(sql: Sql, shortId: string, slot: number, txid: string): boolean {
  return sql.exec('SELECT 1 FROM releases WHERE short_id = ? AND slot = ? AND txid = ?', shortId, slot, txid).toArray().length > 0;
}

/** A stranding row of the transaction for a SPENT slot, held or released. */
function hasSpentRow(sql: Sql, shortId: string, txid: string): boolean {
  return sql.exec("SELECT 1 FROM strandings WHERE short_id = ? AND txid = ? AND kind = 'spent'", shortId, txid).toArray().length > 0;
}

function liveNote(sql: Sql, shortId: string, nonce: string): boolean {
  return sql.exec("SELECT 1 FROM notes WHERE short_id = ? AND nonce = ? AND status = 'live'", shortId, nonce).toArray().length > 0;
}

/**
 * The federation said a note the transaction spent is spent (`recordStranded` marks it spent under
 * the transaction): the transaction landed, so its change is real and is never released.
 */
function landed(sql: Sql, shortId: string, txid: string): boolean {
  return sql.exec("SELECT 1 FROM notes WHERE short_id = ? AND txid = ? AND status = 'spent'", shortId, txid).toArray().length > 0;
}

/**
 * Still true when a stranding's releases are written (signing awaits between the decision and the
 * write): the registry has still never seen the transaction, so a late report that raced the
 * signing wins; every slot is still held for it (no renewal or reset meanwhile); and it has not
 * been found landed.
 */
export const strandingOpen = (shortId: string, txid: string, slots: readonly number[]) => (sql: SqlStorage) =>
  !txKnown(sql, shortId, txid) &&
  !landed(sql, shortId, txid) &&
  slots.every(
    slot => sql.exec('SELECT 1 FROM strandings WHERE short_id = ? AND slot = ? AND txid = ?', shortId, slot, txid).toArray().length > 0,
  );

/** What one report did, by slot, ascending. */
export interface StrandedAnswer {
  /** The report's slots held after it. */
  readonly held: number[];
  /** Slots this report released, any of the card's: listed in its state for the next tap. */
  readonly released: number[];
  /** The report's slots whose note the federation said is spent: marked spent, held no more. */
  readonly spent: number[];
  /** The report's slots not held (see `recordStranded`). */
  readonly ignored: number[];
}

/**
 * `POST /cards/:short_id/stranded` (docs/OFFLINE-PROTOCOL.md § 7): the slots a till found SPENT or
 * CHANGE on the card for a transaction it could not account for, a phone's sale that never synced.
 *
 * One transaction records the rows, `first_seen = now`. A slot is ignored when the registry has
 * seen its transaction (`txKnown`) or released the slot for it; when it has no key; when it is
 * held for another transaction; and, for a SPENT slot, without a live note under its current key,
 * or for a CHANGE slot, when its key ever carried a note (`keyUsed`). A repeat changes nothing.
 * `spent: true` on a spent slot (ENG-867 decision 5) marks its note spent under the transaction and
 * drops the row, at once: a spent note is spent for good.
 *
 * Then each open row past the grace (`STRANDED_GRACE_MS`) is released, per transaction, by
 * `recordReleases`: a spent slot only on its own `unspent: true` in this report; a change slot,
 * unless its transaction landed, when a spent slot of that transaction is `unspent: true` in this
 * report (one unspent input proves the transaction never landed), when the transaction has no
 * spent slot on record (ENG-867 decision 4: no slot signed it), or when it is released already
 * (the gate in `recordOfflineCharge` refuses it for good).
 *
 * No version bump: the notes the card can pay are unchanged until the till sends a release. With
 * no `CERT_SIGNING_KEY` the rows are recorded and nothing is signed.
 */
export async function recordStranded(
  storage: DurableObjectStorage,
  secret: string | undefined,
  shortId: string,
  tillId: number,
  r: Record<string, unknown>,
  now: number,
): Promise<StrandedAnswer> {
  const sql = storage.sql;
  const entries = parseStranded(r, slotCount(sql, shortId));
  const decided = storage.transactionSync(() => {
    const keys = new Map(
      sql
        .exec('SELECT slot, pubkey FROM card_slots WHERE short_id = ?', shortId)
        .toArray()
        .map(k => [Number(k.slot), String(k.pubkey)] as const),
    );
    const open = openStrandings(sql, shortId);
    const held: StrandedSlot[] = [];
    const spent: number[] = [];
    const ignored: number[] = [];
    for (const e of entries) {
      const pubkey = keys.get(e.slot);
      const nonce = pubkey === undefined ? null : '02' + pubkey;
      const takes =
        nonce !== null &&
        !txKnown(sql, shortId, e.txid) &&
        !slotReleased(sql, shortId, e.slot, e.txid) &&
        !open.some(s => s.slot === e.slot && (s.txid !== e.txid || s.kind !== e.kind)) &&
        (e.kind === 'spent' ? liveNote(sql, shortId, nonce) : !keyUsed(sql, shortId, nonce));
      if (!takes) {
        ignored.push(e.slot);
        continue;
      }
      sql.exec(
        'INSERT OR IGNORE INTO strandings (short_id, slot, txid, kind, first_seen, till_id) VALUES (?, ?, ?, ?, ?, ?)',
        shortId, e.slot, e.txid, e.kind, now, tillId,
      );
      if (e.spent) {
        sql.exec("UPDATE notes SET status = 'spent', txid = ? WHERE short_id = ? AND nonce = ? AND status = 'live'", e.txid, shortId, nonce);
        sql.exec('DELETE FROM strandings WHERE short_id = ? AND slot = ? AND txid = ?', shortId, e.slot, e.txid);
        spent.push(e.slot);
        continue;
      }
      held.push(e);
    }
    const unspent = held.filter(e => e.unspent);
    const release = new Map<string, number[]>();
    for (const s of openStrandings(sql, shortId)) {
      if (s.first_seen + STRANDED_GRACE_MS > now || landed(sql, shortId, s.txid)) continue;
      const releasable =
        s.kind === 'spent'
          ? unspent.some(e => e.slot === s.slot && e.txid === s.txid)
          : unspent.some(e => e.txid === s.txid) || !hasSpentRow(sql, shortId, s.txid) || hasRelease(sql, shortId, s.txid);
      if (releasable) release.set(s.txid, [...(release.get(s.txid) ?? []), s.slot]);
    }
    return {held, spent, ignored, release};
  });
  const released: number[] = [];
  for (const [txid, slots] of decided.release) {
    if ((await recordReleases(storage, secret, shortId, txid, slots, strandingOpen(shortId, txid, slots), now)) > 0) {
      released.push(...slots);
    }
  }
  // Read again after the signing: a late report that raced it leaves its rows no longer held.
  const open = new Set(openStrandings(sql, shortId).map(s => `${s.slot}:${s.txid}`));
  const asc = (a: number, b: number) => a - b;
  return {
    held: decided.held.filter(e => open.has(`${e.slot}:${e.txid}`)).map(e => e.slot).sort(asc),
    released: released.sort(asc),
    spent: decided.spent.sort(asc),
    ignored: decided.ignored.sort(asc),
  };
}

/* ------------------------------------------------------------------------- */
/* Reports                                                                   */
/* ------------------------------------------------------------------------- */

/** An operator's offline reports, newest first, for cards in their batches. */
export function listOfflineReports(sql: Sql, operatorId: number, shortId: string | null) {
  return sql
    .exec(
      `SELECT r.txid, r.short_id, r.till_id, r.result, r.detail, r.created_at
       FROM offline_reports r
       JOIN cards c ON c.short_id = r.short_id
       JOIN batches b ON b.batch_id = c.batch_id
       WHERE b.operator_id = ? AND (? IS NULL OR r.short_id = ?)
       ORDER BY r.created_at DESC, r.rowid DESC LIMIT ${REPORTS_LIMIT}`,
      operatorId, shortId, shortId,
    )
    .toArray()
    .map(r => ({
      txid: String(r.txid),
      short_id: String(r.short_id),
      till_id: Number(r.till_id),
      result: String(r.result),
      detail: r.detail === null ? null : (JSON.parse(String(r.detail)) as unknown),
      created_at: Number(r.created_at),
    }));
}
