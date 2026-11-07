/**
 * The object's schema, as forward-only migrations. `schema_version` records
 * how many have run; a new change is a new entry at the end, never an edit.
 *
 * Migration 1 is the schema the first deployment created (2026-10-31), written
 * with IF NOT EXISTS so an object that already has those tables adopts it as
 * version 1 instead of failing.
 */
export const MIGRATIONS: readonly string[] = [
  // 1. the first deployment: batches and card rows.
  `
  CREATE TABLE IF NOT EXISTS batches (
    batch_id      INTEGER PRIMARY KEY AUTOINCREMENT,
    federation_id TEXT    NOT NULL,
    count         INTEGER NOT NULL,
    pin_policy    TEXT    NOT NULL,
    load_sha256   TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
  );
  CREATE TABLE IF NOT EXISTS cards (
    short_id      TEXT    PRIMARY KEY,
    batch_id      INTEGER NOT NULL,
    installed_at  INTEGER NOT NULL,
    load_sha256   TEXT    NOT NULL,
    pin_set       INTEGER NOT NULL,
    pin_policy    TEXT    NOT NULL,
    notes_issued  INTEGER NOT NULL,
    received_at   INTEGER NOT NULL
  );
  `,
  // 2. OPS-PLAN O2: operator accounts as cashucard has them, till tokens, and
  // card state (the keys, notes and charges that lived in the sidecar's
  // card_state.json). Rows from migration 1 were written by the one operator
  // the first deployment had, which the bootstrap creates as id 1.
  `
  CREATE TABLE operators (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
  );
  CREATE TABLE operator_setup_commands (
    nonce         TEXT    PRIMARY KEY,
    action        TEXT    NOT NULL,
    operator_id   INTEGER NOT NULL,
    name          TEXT    NOT NULL,
    applied_at    INTEGER NOT NULL
  );
  CREATE TABLE sessions (
    token_hash    TEXT    PRIMARY KEY,
    operator_id   INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    revoked_at    INTEGER,
    created_at    INTEGER NOT NULL
  );
  CREATE TABLE login_attempt_budget (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    window_start  INTEGER NOT NULL,
    attempts      INTEGER NOT NULL
  );
  ALTER TABLE batches ADD COLUMN operator_id INTEGER NOT NULL DEFAULT 1;
  ALTER TABLE cards ADD COLUMN slot_pubkeys TEXT;
  CREATE TABLE tills (
    till_id       INTEGER PRIMARY KEY AUTOINCREMENT,
    operator_id   INTEGER NOT NULL,
    name          TEXT    NOT NULL,
    token_hash    TEXT    NOT NULL UNIQUE,
    created_at    INTEGER NOT NULL,
    revoked_at    INTEGER
  );
  CREATE TABLE card_slots (
    short_id      TEXT    NOT NULL,
    slot          INTEGER NOT NULL CHECK (slot BETWEEN 0 AND 7),
    pubkey        TEXT    NOT NULL,
    PRIMARY KEY (short_id, slot)
  );
  CREATE TABLE card_versions (
    short_id      TEXT    PRIMARY KEY,
    version       INTEGER NOT NULL
  );
  CREATE TABLE notes (
    short_id      TEXT    NOT NULL,
    nonce         TEXT    NOT NULL,
    slot          INTEGER NOT NULL,
    amount_msat   INTEGER NOT NULL,
    signature     TEXT,
    blinding_key  TEXT,
    status        TEXT    NOT NULL CHECK (status IN ('live', 'pending', 'spent', 'dead')),
    txid          TEXT,
    out_idx       INTEGER,
    PRIMARY KEY (short_id, nonce)
  );
  CREATE TABLE card_txs (
    txid          TEXT    PRIMARY KEY,
    short_id      TEXT    NOT NULL,
    till_id       INTEGER NOT NULL,
    client_ref    TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    status        TEXT    NOT NULL CHECK (status IN ('open', 'accepted', 'rejected')),
    outcome       TEXT,
    created_at    INTEGER NOT NULL,
    resolved_at   INTEGER
  );
  `,
  // v3 (OPS-PLAN O3): the signed bytes a till is about to send, and the
  // federation names tills read from the federation's config.
  `
  ALTER TABLE card_txs ADD COLUMN signed_hex TEXT;
  CREATE TABLE federations (
    operator_id   INTEGER NOT NULL,
    federation_id TEXT    NOT NULL,
    name          TEXT    NOT NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (operator_id, federation_id)
  );
  `,
  // v4 (OPS-PLAN § 8): each backup attempt, for the status route.
  `
  CREATE TABLE backup_runs (
    at         INTEGER NOT NULL,
    ok         INTEGER NOT NULL,
    object_key TEXT,
    bytes      INTEGER,
    error      TEXT
  );
  `,
  // v5 (OPS-PLAN O4): the mint's key per note tier, which every note is checked
  // against before it is live; holder loads in flight; the per-card budget of
  // holder writes. A load's notes live in `notes` like a till's change.
  `
  CREATE TABLE federation_keys (
    operator_id   INTEGER NOT NULL,
    federation_id TEXT    NOT NULL,
    tbs_pks       TEXT    NOT NULL,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (operator_id, federation_id)
  );
  CREATE TABLE holder_loads (
    txid          TEXT    PRIMARY KEY,
    short_id      TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    status        TEXT    NOT NULL CHECK (status IN ('open', 'accepted', 'rejected', 'expired')),
    outcome       TEXT,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    resolved_at   INTEGER
  );
  CREATE INDEX holder_loads_open ON holder_loads (short_id, status);
  CREATE TABLE holder_budget (
    short_id      TEXT    PRIMARY KEY,
    window_start  INTEGER NOT NULL,
    writes        INTEGER NOT NULL
  );
  `,
  // v6 (OPS-PLAN O5): each chip's escrowed card-manager keys (`managerKeys.ts`),
  // the chip a card row was installed on, and a reset's link from the old row
  // to the new one (`reset.ts`).
  `
  CREATE TABLE manager_keys (
    chip          TEXT    PRIMARY KEY,
    operator_id   INTEGER NOT NULL,
    key_version   INTEGER NOT NULL,
    enc           TEXT    NOT NULL,
    mac           TEXT    NOT NULL,
    dek           TEXT    NOT NULL,
    issued_at     INTEGER NOT NULL,
    short_id      TEXT
  );
  ALTER TABLE cards ADD COLUMN chip TEXT;
  ALTER TABLE cards ADD COLUMN reset_at INTEGER;
  ALTER TABLE cards ADD COLUMN replaced_by TEXT;
  `,
  // v7 (OPS-PLAN O5, reset to factory): a reset's completion, proved on the
  // card (`reset.ts`). One row per old card. A reset no longer makes a new
  // card row, so v6's `cards.replaced_by` is left unwritten.
  `
  CREATE TABLE card_resets (
    short_id       TEXT    PRIMARY KEY,
    chip           TEXT    NOT NULL,
    operator_id    INTEGER NOT NULL,
    gp_key_version INTEGER NOT NULL,
    manager_keys   TEXT    NOT NULL CHECK (manager_keys = 'factory'),
    package        TEXT    NOT NULL CHECK (package = 'absent'),
    verified_at    INTEGER NOT NULL,
    completed_at   INTEGER NOT NULL
  );
  `,
  // v8 (docs/TILL-CLAIM.md): till claims, which live 10 minutes, and the per-IP
  // budget for starting them. Neither is in the backup.
  `
  CREATE TABLE till_claims (
    claim_id         TEXT    PRIMARY KEY,
    code             TEXT    NOT NULL UNIQUE,
    name             TEXT    NOT NULL,
    poll_secret_hash TEXT    NOT NULL,
    created_at       INTEGER NOT NULL,
    expires_at       INTEGER NOT NULL,
    approved_by      INTEGER,
    approved_at      INTEGER
  );
  CREATE TABLE till_claim_budget (
    ip_hash       TEXT    PRIMARY KEY,
    window_start  INTEGER NOT NULL,
    claims        INTEGER NOT NULL
  );
  `,
  // v9 (32 slots): each card's own slot count, 8 for every card provisioned
  // before it, and `card_slots` rebuilt because SQLite cannot alter v2's
  // CHECK (slot BETWEEN 0 AND 7). The rows are copied as they are; the table
  // has no index beyond its key and nothing references it.
  `
  ALTER TABLE cards ADD COLUMN slot_count INTEGER NOT NULL DEFAULT 8 CHECK (slot_count IN (8, 32));
  CREATE TABLE card_slots_v9 (
    short_id      TEXT    NOT NULL,
    slot          INTEGER NOT NULL CHECK (slot BETWEEN 0 AND 31),
    pubkey        TEXT    NOT NULL,
    PRIMARY KEY (short_id, slot)
  );
  INSERT INTO card_slots_v9 (short_id, slot, pubkey) SELECT short_id, slot, pubkey FROM card_slots;
  DROP TABLE card_slots;
  ALTER TABLE card_slots_v9 RENAME TO card_slots;
  `,
  // v10 (docs/OFFLINE-PROTOCOL.md § 3): a card's attestation key with the
  // certificate issued for it (stored, because ECDSA signatures are randomized
  // and the same key must answer the same bytes), and the log of charges a card
  // signed offline. The key is (txid, till_id): one report per txid per till, so
  // a till retrying is idempotent, while two tills reporting the same txid each
  // leave their own row.
  `
  ALTER TABLE cards ADD COLUMN attest_pub TEXT;
  ALTER TABLE cards ADD COLUMN attest_cert TEXT;
  CREATE TABLE offline_reports (
    txid       TEXT    NOT NULL,
    short_id   TEXT    NOT NULL,
    till_id    INTEGER NOT NULL,
    result     TEXT    NOT NULL CHECK (result IN ('recorded', 'double_spend')),
    detail     TEXT,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (txid, till_id)
  );
  CREATE INDEX offline_reports_card ON offline_reports (short_id, created_at);
  `,
];

/** Runs every migration not yet applied, each with its version bump. */
export function migrate(storage: DurableObjectStorage): void {
  const sql = storage.sql;
  sql.exec('CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)');
  const row = sql.exec('SELECT version FROM schema_version').toArray()[0] as
    | {version: number}
    | undefined;
  let version = row ? Number(row.version) : 0;
  if (!row) {
    sql.exec('INSERT INTO schema_version (version) VALUES (0)');
  }
  while (version < MIGRATIONS.length) {
    const next = MIGRATIONS[version]!;
    storage.transactionSync(() => {
      sql.exec(next);
      sql.exec('UPDATE schema_version SET version = ?', version + 1);
    });
    version += 1;
  }
}
