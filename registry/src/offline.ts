/**
 * Offline card payments, the registry's half (docs/OFFLINE-PROTOCOL.md § 2-3).
 *
 * Three things live here: the certificate that vouches for a card's attestation
 * key (signed with `CERT_SIGNING_KEY`), the route a till uses to report a charge
 * the card signed with no connection, and the report log an operator reads.
 *
 * The registry cannot stop an offline double spend; the federation's spent set
 * does. What it does is decide, once the till is back online, whether the
 * card's inputs were still live. If so the charge enters the normal
 * intent/submitted/outcome flow already open and signed; if not it is refused
 * with the conflicting notes and kept as a report.
 */
import {bump, cardOwner, MAX_TX_HEX, parseIntent, slotCount, versionOf, type ChangeOut} from './cards';
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
    return recordPendingAttestation(storage, secret, operatorId, shortId, named, attest, attestHex, Date.now());
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
  | {recorded: false; conflicts: Conflicts[]};

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
      return {recorded: false, conflicts: (JSON.parse(String(reported.detail)) as {conflicts: Conflicts[]}).conflicts};
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
    const report = (result: 'recorded' | 'double_spend', detail: unknown) =>
      sql.exec(
        'INSERT INTO offline_reports (txid, short_id, till_id, result, detail, created_at) VALUES (?, ?, ?, ?, ?, ?)',
        intent.txid, shortId, tillId, result, JSON.stringify(detail), now,
      );
    if (conflicts.length > 0) {
      report('double_spend', {client_ref: intent.client_ref, signed_at: r.signed_at, inputs: intent.body.inputs, conflicts});
      return {recorded: false, conflicts};
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
