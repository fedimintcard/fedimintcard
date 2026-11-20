/**
 * The phone's secure random source: `crypto.getRandomValues`, which index.js
 * installs before anything else loads (react-native-get-random-values, which
 * in a release build answers from SecRandomCopyBytes on iOS and
 * java.security.SecureRandom on Android).
 *
 * The app draws two things from it: the host challenge of every secure
 * channel it opens to a card (cardkit's `Random`, which LoopScreen and
 * ResetScreen hand in) and the digits of a generated PIN (`pin.ts`). There is
 * no fallback. A phone without the source gets an error, so no card is opened
 * with a guessable challenge and no holder is handed a guessable PIN.
 * `secureRandom.test.ts` fails if an app source draws from the engine's own,
 * non-cryptographic random again.
 */

/** The most one `getRandomValues` call fills (the Web Crypto limit). */
export const MAX_RANDOM_BYTES = 65_536;

type PlatformCrypto = {
  getRandomValues?: (array: Uint8Array | Uint32Array) => unknown;
};

/** `array`, filled by the platform source; throws when the phone has none. */
function fromPlatform<T extends Uint8Array | Uint32Array>(array: T): T {
  const c = (globalThis as {crypto?: PlatformCrypto | null}).crypto;
  if (typeof c?.getRandomValues !== 'function') {
    throw new Error('no platform random source (crypto.getRandomValues)');
  }
  c.getRandomValues(array);
  return array;
}

/**
 * `length` bytes from the platform source: cardkit's `Random`, the secure
 * channel's host challenge. A length that is not a whole number from 0 to
 * `MAX_RANDOM_BYTES` is refused, where a typed array would quietly round it.
 */
export function secureRandomBytes(length: number): Uint8Array {
  if (!Number.isInteger(length) || length < 0 || length > MAX_RANDOM_BYTES) {
    throw new RangeError(
      `a random draw is a whole number of bytes from 0 to ${MAX_RANDOM_BYTES}, not ${length}`,
    );
  }
  return fromPlatform(new Uint8Array(length));
}

/** Uniform in [0, 1), in steps of 2^-32, from one 32-bit draw of the platform source. */
export function secureRandomUnit(): number {
  return fromPlatform(new Uint32Array(1))[0] / 0x1_0000_0000;
}
