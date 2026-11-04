#!/usr/bin/env node
// Restore drill: fetch one backup from R2 and decrypt it with the age identity
// kept outside Cloudflare, then print what it holds (counts, never rows).
//   node scripts/backup-drill.mjs <bucket> <object key> [identity file]
// The key is `last_key` from GET /registry/backup-status.
import {execFileSync} from 'node:child_process';
import {mkdtempSync, readFileSync, rmSync} from 'node:fs';
import {homedir, tmpdir} from 'node:os';
import {join} from 'node:path';
import * as age from 'age-encryption';

const [bucket, key, idPath = join(homedir(), '.config/fedimintcard/backup-identity.txt')] = process.argv.slice(2);
if (!bucket || !key) {
  console.error('usage: backup-drill.mjs <bucket> <object key> [identity file]');
  process.exit(2);
}
const dir = mkdtempSync(join(tmpdir(), 'registry-drill-'));
try {
  const file = join(dir, 'backup.age');
  execFileSync('npx', ['wrangler', 'r2', 'object', 'get', `${bucket}/${key}`, '--file', file, '--remote'], {stdio: 'ignore'});
  const identity = readFileSync(idPath, 'utf8').split('\n').find(l => l.startsWith('AGE-SECRET-KEY-'));
  const d = new age.Decrypter();
  d.addIdentity(identity);
  const snap = JSON.parse(await d.decrypt(age.armor.decode(readFileSync(file, 'utf8')), 'text'));
  console.log(`snapshot v${snap.snapshot_version}, schema v${snap.schema_version}, taken ${new Date(snap.created_at).toISOString()}`);
  for (const [t, rows] of Object.entries(snap.tables)) console.log(`  ${t}: ${rows.length}`);
} finally {
  rmSync(dir, {recursive: true, force: true});
}
