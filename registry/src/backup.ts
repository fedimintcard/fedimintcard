/**
 * The nightly backup (OPS-PLAN § 8: ops is the only copy of card state).
 *
 * Shape follows cashucard's escrow export: a versioned JSON snapshot of the
 * registry, encrypted to an age recipient whose identity is kept outside
 * this Cloudflare account, written to R2 under a unique key. Nothing in it is
 * spendable without the card, but it maps cards to balances, so it is
 * encrypted anyway.
 *
 * It carries the card-manager key escrow (`manager_keys`, OPS-PLAN O5) on
 * purpose: a restore without it would leave every rotated card unmanageable.
 * Whoever holds a backup and its age identity can therefore delete and
 * reinstall the applet on every card in it (never spend from one).
 *
 * Left out: password and till-token hashes, sessions, the sign-in budget and
 * applied setup commands. After a restore, operators sign in again through an
 * OPERATOR_SETUP `recover` and tills are issued new tokens.
 */
import * as age from 'age-encryption';

export const SNAPSHOT_VERSION = 1;
/** A backup older than this reads as stale; the alarm runs every 24 hours. */
export const STALE_AFTER_MS = 26 * 60 * 60 * 1000;

const TABLES = [
  'operators', 'batches', 'cards', 'card_slots', 'card_versions', 'notes', 'card_txs',
  'tills', 'federations', 'federation_keys', 'holder_loads', 'manager_keys', 'card_resets',
] as const;
/** Columns that never leave the object. */
const SECRET_COLUMNS = new Set(['password_hash', 'token_hash']);

export interface Snapshot {
  readonly snapshot_version: number;
  readonly created_at: number;
  readonly schema_version: number;
  readonly tables: Record<string, Record<string, unknown>[]>;
}

/** Every table, read in one synchronous pass, so the copy is consistent. */
export function snapshot(storage: DurableObjectStorage, now: number): Snapshot {
  const sql = storage.sql;
  return storage.transactionSync(() => {
    const tables: Record<string, Record<string, unknown>[]> = {};
    for (const t of TABLES) {
      tables[t] = sql
        .exec(`SELECT * FROM ${t}`)
        .toArray()
        .map(row => Object.fromEntries(Object.entries(row).filter(([k]) => !SECRET_COLUMNS.has(k))));
    }
    const v = sql.exec('SELECT version FROM schema_version').one() as {version: number};
    return {snapshot_version: SNAPSHOT_VERSION, created_at: now, schema_version: Number(v.version), tables};
  });
}

export async function encrypt(snap: Snapshot, recipient: string): Promise<string> {
  const e = new age.Encrypter();
  e.addRecipient(recipient);
  return age.armor.encode(await e.encrypt(JSON.stringify(snap)));
}

/** The restore drill's read: an armored backup and its age identity. */
export async function decrypt(armored: string, identity: string): Promise<Snapshot> {
  const d = new age.Decrypter();
  d.addIdentity(identity);
  return JSON.parse(await d.decrypt(age.armor.decode(armored), 'text')) as Snapshot;
}

/** The next 03:17 UTC after `now`: quiet hours for a Caribbean till. */
export function nextBackupAt(now: number): number {
  const d = new Date(now);
  const at = Date.UTC(d.getUTCFullYear(), d.getUTCMonth(), d.getUTCDate(), 3, 17);
  return at > now ? at : at + 24 * 60 * 60 * 1000;
}

export function backupKey(now: number): string {
  return `backups/${new Date(now).toISOString().replace(/[:.]/g, '-')}-${crypto.randomUUID()}.json.age`;
}

/** Runs one backup and records the result; throws so the alarm retries. */
export async function runBackup(
  storage: DurableObjectStorage,
  bucket: R2Bucket | undefined,
  recipient: string | undefined,
  now: number,
): Promise<{key: string; bytes: number}> {
  const sql = storage.sql;
  try {
    if (!bucket || !recipient?.startsWith('age1')) {
      throw new Error('backup is not configured: bind BACKUPS and set BACKUP_RECIPIENT (age1…)');
    }
    const armored = await encrypt(snapshot(storage, now), recipient);
    const key = backupKey(now);
    await bucket.put(key, armored, {httpMetadata: {contentType: 'application/octet-stream'}});
    sql.exec(
      "INSERT INTO backup_runs (at, ok, object_key, bytes, error) VALUES (?, 1, ?, ?, NULL)",
      now, key, armored.length,
    );
    return {key, bytes: armored.length};
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    sql.exec("INSERT INTO backup_runs (at, ok, object_key, bytes, error) VALUES (?, 0, NULL, NULL, ?)", now, message.slice(0, 500));
    throw e;
  }
}

export interface BackupStatus {
  readonly configured: boolean;
  readonly last_success_at: number | null;
  readonly last_key: string | null;
  readonly last_error: string | null;
  readonly stale: boolean;
}

export function backupStatus(sql: SqlStorage, configured: boolean, now: number): BackupStatus {
  const ok = sql.exec('SELECT at, object_key FROM backup_runs WHERE ok = 1 ORDER BY at DESC LIMIT 1').toArray()[0] as
    | {at: number; object_key: string}
    | undefined;
  const last = sql.exec('SELECT ok, error FROM backup_runs ORDER BY at DESC LIMIT 1').toArray()[0] as
    | {ok: number; error: string | null}
    | undefined;
  const lastAt = ok ? Number(ok.at) : null;
  return {
    configured,
    last_success_at: lastAt,
    last_key: ok ? String(ok.object_key) : null,
    last_error: last && Number(last.ok) === 0 ? String(last.error) : null,
    stale: lastAt === null || now - lastAt > STALE_AFTER_MS,
  };
}
