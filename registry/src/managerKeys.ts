/**
 * Per-card card-manager keys (OPS-PLAN O5, after cashucard's D29): each card's
 * GlobalPlatform ENC/MAC/DEK set, derived here and escrowed in the same
 * transaction that issues it, so the phone that rotates a card's keys never
 * needs to keep them.
 *
 *   POST /registry/manager-keys  {chip, short_id?}  -> {chip, key_version, enc, mac, dek}
 *
 * - **Derivation.** HKDF-SHA256 over the master (`CARD_MANAGER_MASTER`, a
 *   Worker secret, 32 bytes hex), salt `fedimintcard/card-manager/v<version>`,
 *   info `gp-enc`|`gp-mac`|`gp-dek` followed by the chip id, 16 bytes each:
 *   the SCP02 3DES keys the J3R180 takes. The chip id is the card's CPLC
 *   (`tools/cardkit/gp/identity.ts`), written by the chip maker, so a card
 *   keeps the same set across a reset; its short id does not survive one.
 * - **Escrow.** The derived bytes are stored with the chip, the operator who
 *   first asked, the key version and when. A repeat for the same chip answers
 *   the stored set, so a phone that lost the answer (or the card's PUT KEY
 *   reply) gets byte-identical keys, and a set never exists on a card without
 *   existing here first. The stored set also outlives a change of master.
 * - **Ownership.** The first operator to ask for a chip owns it. Another
 *   operator gets a 409 that says so and no keys. Not a 404: the caller is
 *   holding the chip, so its existence is no secret, and "no such chip" would
 *   send them looking for a bug instead of the card's owner.
 * - **No master, no keys.** Without the secret this route answers 503 and
 *   nothing else changes, so a deployment can run without it until its first
 *   card is rotated.
 *
 * The keys are never logged. They are in the nightly backup, which is
 * age-encrypted: whoever holds the escrow, or the backup and its identity, can
 * manage (delete and reinstall) every card in it. They cannot spend from one:
 * a note is spent only by the card's slot key, behind the holder's PIN.
 */
import {hkdf} from '@noble/hashes/hkdf.js';
import {sha256} from '@noble/hashes/sha2.js';

import {BadRequest, Conflict, isShortId, NotFound, ServiceUnavailable, toHex} from './http';

type Sql = SqlStorage;

/** The derivation this code writes. A later scheme is a new version, never an edit. */
export const KEY_VERSION = 1;

/** A chip's CPLC identity, as the app reads it (cardkit's `CHIP_ID_PATTERN`). */
export const CHIP_PATTERN = /^cplc-v1:[0-9a-f]{24}$/;

export function isChip(value: unknown): value is string {
  return typeof value === 'string' && CHIP_PATTERN.test(value);
}

export interface ManagerKeys {
  readonly chip: string;
  readonly key_version: number;
  /** 16 bytes each, hex. */
  readonly enc: string;
  readonly mac: string;
  readonly dek: string;
}

/** The master's 32 bytes, or null when the secret is missing or not 64 hex characters. */
export function parseMaster(hex: string | undefined): Uint8Array | null {
  const cleaned = hex?.trim().toLowerCase() ?? '';
  if (!/^[0-9a-f]{64}$/.test(cleaned)) {
    return null;
  }
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i += 1) {
    out[i] = parseInt(cleaned.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/** Pure: the same master, chip and version give the same 48 bytes, forever. */
export function deriveManagerKeys(master: Uint8Array, chip: string, version = KEY_VERSION): ManagerKeys {
  if (master.length !== 32) {
    throw new Error('the master is 32 bytes');
  }
  if (!isChip(chip)) {
    throw new Error('not a chip id');
  }
  const encoder = new TextEncoder();
  const salt = encoder.encode(`fedimintcard/card-manager/v${version}`);
  const key = (label: string) => toHex(hkdf(sha256, master, salt, encoder.encode(`${label}${chip}`), 16));
  return {
    chip,
    key_version: version,
    enc: key('gp-enc'),
    mac: key('gp-mac'),
    dek: key('gp-dek'),
  };
}

interface EscrowRow {
  chip: string;
  operator_id: number;
  key_version: number;
  enc: string;
  mac: string;
  dek: string;
  short_id: string | null;
}

function escrowed(sql: Sql, chip: string): EscrowRow | null {
  const row = sql.exec('SELECT * FROM manager_keys WHERE chip = ?', chip).toArray()[0];
  if (!row) {
    return null;
  }
  return {
    chip: String(row.chip),
    operator_id: Number(row.operator_id),
    key_version: Number(row.key_version),
    enc: String(row.enc),
    mac: String(row.mac),
    dek: String(row.dek),
    short_id: row.short_id === null ? null : String(row.short_id),
  };
}

/**
 * `POST /registry/manager-keys`. Derive and escrow in one transaction, or
 * answer what is escrowed. `short_id`, when the phone knows it (a reset reads
 * it off the card), must be this operator's card, and is recorded against the
 * chip.
 */
export function issueManagerKeys(
  storage: DurableObjectStorage,
  operatorId: number,
  masterHex: string | undefined,
  r: Record<string, unknown>,
  now: number,
  /** The operator who owns a card, through its batch (`cards.ts` `cardOwner`). */
  ownerOf: (shortId: string) => number | null,
): ManagerKeys {
  const master = parseMaster(masterHex);
  if (!master) {
    throw new ServiceUnavailable('card-manager keys are not configured on this registry (CARD_MANAGER_MASTER)');
  }
  if (!isChip(r.chip)) {
    throw new BadRequest('chip must be the card’s CPLC id, cplc-v1: and 24 lower-case hex characters');
  }
  const chip = r.chip;
  const shortId = r.short_id;
  if (shortId !== undefined && !isShortId(shortId)) {
    throw new BadRequest('short_id must be 8 lower-case hex characters');
  }
  const sql = storage.sql;
  if (shortId !== undefined && ownerOf(shortId) !== operatorId) {
    throw new NotFound(`no card ${shortId}`);
  }
  // Outside the transaction: pure, and the same bytes whoever wins.
  const derived = deriveManagerKeys(master, chip);
  return storage.transactionSync(() => {
    const existing = escrowed(sql, chip);
    if (existing && existing.operator_id !== operatorId) {
      throw new Conflict('this card’s manager keys belong to another operator');
    }
    if (!existing) {
      sql.exec(
        `INSERT INTO manager_keys (chip, operator_id, key_version, enc, mac, dek, issued_at, short_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
        chip,
        operatorId,
        derived.key_version,
        derived.enc,
        derived.mac,
        derived.dek,
        now,
        shortId ?? null,
      );
      return derived;
    }
    if (shortId !== undefined && existing.short_id !== shortId) {
      linkChip(sql, chip, shortId, operatorId);
    }
    // The escrowed set, not a fresh derivation: it is what the card was given.
    return {
      chip,
      key_version: existing.key_version,
      enc: existing.enc,
      mac: existing.mac,
      dek: existing.dek,
    };
  });
}

/**
 * Records the card a chip carries now, on that operator's escrow row only.
 * Inside the caller's transaction; a chip with no escrow row is left alone.
 */
export function linkChip(sql: Sql, chip: string, shortId: string, operatorId: number): void {
  sql.exec('UPDATE manager_keys SET short_id = ? WHERE chip = ? AND operator_id = ?', shortId, chip, operatorId);
}
