// A deployment-owner grant reads encrypted registry snapshots across operators.
// It deliberately has no operator/till permissions and cannot create a backup.
import {bearer, constantTimeEqual, fail, fromHex, isHex, json, sha256Hex} from './http';
import type {Env} from './index';

const UUID = '[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}';
const identifier = new RegExp('^' + UUID + '$');
const code = /^[A-Za-z0-9_-]{43}$/;
const keyPattern = new RegExp(
  '^backups/(\\d{4}-\\d{2}-\\d{2}T\\d{2})-(\\d{2})-(\\d{2})-(\\d{3}Z)-(' + UUID + ')\\.json\\.age$',
);
const exportPattern = new RegExp('^(\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}\\.\\d{3}Z)-(' + UUID + ')\\.age$');
const readerKey = 'atoll-recovery-reader-v1';

interface Grant {
  issuer_id: string;
  code_hash: string;
  expires_at: number;
}
interface Reader {
  issuer: string;
  grant: string;
  digest: string;
  connection: string;
}

function grant(raw?: string): Grant | null {
  if (!raw) return null;
  try {
    const g = JSON.parse(raw);
    if (
      g &&
      typeof g.issuer_id === 'string' &&
      identifier.test(g.issuer_id) &&
      isHex(g.code_hash, 32) &&
      Number.isSafeInteger(g.expires_at)
    )
      return g;
  } catch {
    /* A partial deployment must keep the reader disabled. */
  }
  return null;
}

function equalHash(a: string, b: string): boolean {
  return constantTimeEqual(fromHex(a), fromHex(b));
}

// Only the two small strings emitted by the Atoll pairing action belong here.
async function claimBody(request: Request): Promise<{code: string; secret: string} | null> {
  const reader = request.body?.getReader();
  if (!reader) return null;
  const buf = new Uint8Array(512);
  let length = 0;
  for (;;) {
    const chunk = await reader.read();
    if (chunk.done) break;
    if (chunk.value.length > buf.length - length) {
      await reader.cancel();
      return null;
    }
    buf.set(chunk.value, length);
    length += chunk.value.length;
  }
  try {
    const b = JSON.parse(new TextDecoder().decode(buf.subarray(0, length)));
    if (typeof b?.code === 'string' && typeof b?.secret === 'string' && code.test(b.code) && code.test(b.secret))
      return b;
  } catch {
    /* Refuse malformed input without logging it. */
  }
  return null;
}

export async function recovery(request: Request, env: Env, storage: DurableObjectStorage): Promise<Response> {
  const g = grant(env.ATOLL_RECOVERY_GRANT);
  const bucket = env.BACKUPS;
  if (!g || !bucket) return fail(404, 'no recovery reader configured');
  const url = new URL(request.url);
  if (url.pathname === '/recovery/claim' && request.method === 'POST') {
    const b = await claimBody(request);
    if (!b || !equalHash(await sha256Hex(b.code), g.code_hash)) return fail(403, 'claim refused');
    const digest = await sha256Hex(b.secret);
    const connection = await storage.transaction(async (tx) => {
      const old = await tx.get<Reader>(readerKey);
      if (old?.grant === g.code_hash && old.issuer === g.issuer_id) {
        return equalHash(old.digest, digest) ? old.connection : null;
      }
      if (Date.now() >= g.expires_at) return null;
      const id = crypto.randomUUID();
      await tx.put<Reader>(readerKey, {issuer: g.issuer_id, grant: g.code_hash, digest, connection: id});
      return id;
    });
    return connection
      ? json(200, {issuer_id: g.issuer_id, connection_id: connection})
      : fail(403, 'claim refused');
  }

  const token = /^recovery-v1\.([A-Za-z0-9_-]{43})$/.exec(bearer(request));
  if (!token || request.method !== 'GET') return fail(403, 'reader required');
  const saved = await storage.get<Reader>(readerKey);
  if (
    !saved ||
    saved.issuer !== g.issuer_id ||
    saved.grant !== g.code_hash ||
    !equalHash(await sha256Hex(token[1]!), saved.digest)
  )
    return fail(403, 'reader required');

  if (url.pathname === '/recovery/exports') {
    const cursor = url.searchParams.get('cursor');
    if (cursor && cursor.length > 1024) return fail(400, 'invalid cursor');
    const page = await bucket.list({prefix: 'backups/', cursor: cursor ?? undefined, limit: 1000});
    const objects = page.objects.flatMap((o) => {
      const m = keyPattern.exec(o.key);
      return m
        ? [{Name: m[1] + ':' + m[2] + ':' + m[3] + '.' + m[4] + '-' + m[5] + '.age', Size: o.size, IsDir: false}]
        : [];
    });
    return json(200, {issuer_id: g.issuer_id, objects, cursor: page.truncated ? page.cursor : null});
  }
  const prefix = '/recovery/exports/';
  if (!url.pathname.startsWith(prefix)) return fail(404, 'unknown recovery route');
  let name: string;
  try {
    name = decodeURIComponent(url.pathname.slice(prefix.length));
  } catch {
    return fail(400, 'invalid export name');
  }
  const m = exportPattern.exec(name);
  if (!m) return fail(404, 'unknown export');
  const object = await bucket.get('backups/' + m[1]!.replace(/[:.]/g, '-') + '-' + m[2] + '.json.age');
  if (!object) return fail(404, 'unknown export');
  return new Response(object.body, {
    headers: {'Content-Type': 'application/octet-stream', 'Cache-Control': 'no-store'},
  });
}
