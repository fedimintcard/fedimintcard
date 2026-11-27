#!/usr/bin/env node
// Local rehearsal with Node >=22.18. Nothing is written to a registry or to disk.
import {readFile} from 'node:fs/promises';
import {createHash} from 'node:crypto';
import {DatabaseSync} from 'node:sqlite';
import {decrypt} from '../src/backup.ts';
import {migrate} from '../src/db.ts';
import {restoreSnapshot} from './restore-snapshot.ts';

const [archivePath, identityPath, receiptPath] = process.argv.slice(2);
if (!archivePath || !identityPath || !receiptPath || process.argv.length !== 5) {
  throw new Error(
    'Usage: node scripts/restore-nextcloud-copy.mjs <archive.age> <age-identity-file> <archive.age.json>',
  );
}
const bytes = await readFile(archivePath);
const receipt = JSON.parse(await readFile(receiptPath, 'utf8'));
if (receipt.size !== bytes.length || createHash('sha256').update(bytes).digest('hex') !== receipt.sha256) {
  throw new Error('Archive does not match its Atoll copy receipt');
}
const identity = (await readFile(identityPath, 'utf8'))
  .split(/\r?\n/)
  .find((line) => line.startsWith('AGE-SECRET-KEY-'));
if (!identity) throw new Error('No age identity in the supplied file');
const snap = await decrypt(bytes.toString('utf8'), identity);
const db = new DatabaseSync(':memory:');
// Adapt just the synchronous SQLite methods used by the registry's own
// migrations and importer. The application never gets the recovery identity.
const storage = {
  sql: {
    exec(statement, ...bindings) {
      if (!bindings.length && !/^\s*(SELECT|PRAGMA)/i.test(statement)) {
        db.exec(statement);
        return;
      }
      const rows = db.prepare(statement).all(...bindings);
      return {
        toArray: () => rows,
        one() {
          if (rows.length !== 1) throw new Error('Expected one row');
          return rows[0];
        },
      };
    },
  },
  transactionSync(fn) {
    db.exec('BEGIN');
    try {
      const result = fn();
      db.exec('COMMIT');
      return result;
    } catch (err) {
      db.exec('ROLLBACK');
      throw err;
    }
  },
};
try {
  migrate(storage);
  restoreSnapshot(storage, snap);
  for (const table of Object.keys(snap.tables)) {
    const {n} = db.prepare('SELECT COUNT(*) AS n FROM ' + table).get();
    if (n !== snap.tables[table].length) throw new Error('Row count mismatch');
    console.log(table + ': ' + n);
  }
  if (db.prepare('PRAGMA integrity_check').get().integrity_check !== 'ok')
    throw new Error('SQLite integrity check failed');
  console.log('Restore verified in a fresh in-memory registry database');
} finally {
  db.close();
}
