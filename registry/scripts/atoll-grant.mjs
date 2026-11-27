#!/usr/bin/env node
import {createHash, randomBytes} from 'node:crypto';

const [issuerId] = process.argv.slice(2);
if (
  process.argv.length !== 3 ||
  !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(issuerId)
) {
  throw new Error('Usage: node scripts/atoll-grant.mjs <stable-registry-uuid>');
}
const code = randomBytes(32).toString('base64url');
console.error('Private Atoll pairing code (valid for 10 minutes): ' + code);
console.log(
  JSON.stringify({
    issuer_id: issuerId,
    code_hash: createHash('sha256').update(code).digest('hex'),
    expires_at: Date.now() + 10 * 60_000,
  }),
);
