// Offline drill only. There is deliberately no public restore route.
import {SNAPSHOT_TABLES, SNAPSHOT_VERSION, type Snapshot} from '../src/backup.ts';
import {MIGRATIONS} from '../src/db.ts';

export function restoreSnapshot(storage: DurableObjectStorage, snap: Snapshot, now = Date.now()): void {
  if (
    !Number.isSafeInteger(snap.created_at) ||
    snap.created_at <= 0 ||
    snap.snapshot_version !== SNAPSHOT_VERSION ||
    snap.schema_version !== MIGRATIONS.length ||
    !snap.tables ||
    Object.keys(snap.tables).length !== SNAPSHOT_TABLES.length ||
    !SNAPSHOT_TABLES.every((t) => Array.isArray(snap.tables[t]))
  )
    throw new Error('Unsupported or incomplete snapshot');

  const sql = storage.sql;
  storage.transactionSync(() => {
    if (sql.exec<{version: number}>('SELECT version FROM schema_version').one().version !== snap.schema_version) {
      throw new Error('Target schema differs from snapshot');
    }
    // Even an expired session or a pending transaction disqualifies this target.
    const existing = sql.exec<{name: string}>("SELECT name FROM sqlite_master WHERE type = 'table'").toArray();
    for (const {name} of existing) {
      if (name === 'schema_version' || name.startsWith('sqlite_') || name.startsWith('_cf_')) continue;
      if (!/^[a-z_]+$/.test(name)) throw new Error('Unexpected target table');
      if (sql.exec('SELECT 1 FROM "' + name + '" LIMIT 1').toArray().length)
        throw new Error('Target registry must be empty');
    }
    for (const table of SNAPSHOT_TABLES) {
      // Identifiers come from the migrated registry, never from snapshot JSON.
      const columns = sql
        .exec<{name: string}>('PRAGMA table_info(' + table + ')')
        .toArray()
        .map((c) => c.name);
      const exported = columns.filter((c) => c !== 'password_hash' && c !== 'token_hash');
      const insert =
        'INSERT INTO ' +
        table +
        ' (' +
        columns.join(', ') +
        ') VALUES (' +
        columns.map(() => '?').join(', ') +
        ')';
      for (const row of snap.tables[table]!) {
        if (
          !row ||
          typeof row !== 'object' ||
          Object.keys(row).length !== exported.length ||
          !exported.every((c) => Object.hasOwn(row, c))
        )
          throw new Error('Invalid snapshot row in ' + table);
        const values = columns.map((c) => {
          if (c === 'password_hash') return 'disabled';
          if (c === 'token_hash') return 'disabled:' + row.till_id;
          if (table === 'tills' && c === 'revoked_at') return row.revoked_at ?? now;
          return row[c];
        });
        if (
          !values.every(
            (v): v is number | string | null =>
              v === null || typeof v === 'string' || (typeof v === 'number' && Number.isFinite(v)),
          )
        ) {
          throw new Error('Invalid snapshot value in ' + table);
        }
        sql.exec(insert, ...values);
      }
    }
  });
}
