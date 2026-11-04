/**
 * Response helpers and the field validators every route shares. The app reads
 * JSON on every status and shows `reason` off a failure
 * (`apps/provisioner/src/api.ts` `reasonOf`), so every answer here is JSON.
 */

export function json(status: number, body: unknown): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'Content-Type': 'application/json; charset=utf-8',
      'Cache-Control': 'no-store',
    },
  });
}

/** A failure the app can show. */
export function fail(status: number, reason: string): Response {
  return json(status, {ok: false, reason});
}

/** The parsed JSON body, or null for no body, bad JSON or a non-object. */
export async function objectBody(
  request: Request,
): Promise<Record<string, unknown> | null> {
  const raw: unknown = await request.json().catch(() => null);
  return typeof raw === 'object' && raw !== null && !Array.isArray(raw)
    ? (raw as Record<string, unknown>)
    : null;
}

export function bearer(request: Request): string {
  const header = request.headers.get('Authorization') ?? '';
  return header.startsWith('Bearer ') ? header.slice(7).trim() : '';
}

/** Lower-case hex of exactly `bytes` bytes. */
export function isHex(value: unknown, bytes: number): value is string {
  return (
    typeof value === 'string' &&
    value.length === bytes * 2 &&
    /^[0-9a-f]*$/.test(value)
  );
}

/**
 * A card's short id: the last 4 bytes of its slot-0 x-only key, lower-case
 * hex (the app's `shortIdOf`). Eight characters, no prefix.
 */
export function isShortId(value: unknown): value is string {
  return isHex(value, 4);
}

export function isPinPolicy(value: unknown): value is 'batch' | 'perCard' {
  return value === 'batch' || value === 'perCard';
}

export function isPositiveInt(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) > 0;
}

export function isCount(value: unknown): value is number {
  return Number.isSafeInteger(value) && (value as number) >= 0;
}

export function toHex(bytes: Uint8Array): string {
  let out = '';
  for (const byte of bytes) {
    out += byte.toString(16).padStart(2, '0');
  }
  return out;
}

export function fromHex(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i += 1) {
    out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/** Equal-length byte comparison whose time does not depend on where they differ. */
export function constantTimeEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) {
    return false;
  }
  let diff = 0;
  for (let i = 0; i < a.length; i += 1) {
    diff |= a[i]! ^ b[i]!;
  }
  return diff === 0;
}

export async function sha256Hex(text: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    'SHA-256',
    new TextEncoder().encode(text),
  );
  return toHex(new Uint8Array(digest));
}

export function randomHex(bytes: number): string {
  return toHex(crypto.getRandomValues(new Uint8Array(bytes)));
}
