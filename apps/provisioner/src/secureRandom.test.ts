/**
 * The phone's secure random source and the two things the app draws from it:
 * the host challenge of every secure channel opened to a card (LoopScreen and
 * ResetScreen hand `secureRandomBytes` to cardkit) and the digits of a
 * generated PIN (`generatePin`). With no platform source each of them
 * refuses; none of them falls back to a guessable source.
 */
import * as fs from 'fs';
import * as path from 'path';

import {GENERATED_PIN_LENGTH, generatePin, isValidPin} from './pin';
import {
  MAX_RANDOM_BYTES,
  secureRandomBytes,
  secureRandomUnit,
} from './secureRandom';

afterEach(() => {
  jest.restoreAllMocks();
});

describe('secureRandomBytes', () => {
  test('fills a fresh Uint8Array of the asked length from crypto.getRandomValues', () => {
    const spy = jest.spyOn(globalThis.crypto, 'getRandomValues');
    const out = secureRandomBytes(8);
    expect(spy).toHaveBeenCalledTimes(1);
    const filled = spy.mock.calls[0][0];
    expect(filled).toBeInstanceOf(Uint8Array);
    expect(filled).toHaveLength(8);
    expect(out).toBe(filled);
  });

  test('takes every whole length one platform call can fill', () => {
    expect(MAX_RANDOM_BYTES).toBe(65_536);
    expect(secureRandomBytes(0)).toHaveLength(0);
    expect(secureRandomBytes(MAX_RANDOM_BYTES)).toHaveLength(MAX_RANDOM_BYTES);
  });

  // A typed array would quietly round 2.5 down and take NaN as 0: fewer
  // challenge bytes than cardkit asked for, with no error.
  test.each([-1, 2.5, NaN, Infinity, MAX_RANDOM_BYTES + 1])(
    'refuses a length of %p before asking the platform',
    length => {
      const spy = jest.spyOn(globalThis.crypto, 'getRandomValues');
      expect(() => secureRandomBytes(length)).toThrow(RangeError);
      expect(spy).not.toHaveBeenCalled();
    },
  );
});

describe('secureRandomUnit', () => {
  test.each([0, 0xffff_ffff])(
    'is in [0, 1) when the platform yields %d',
    word => {
      const spy = jest
        .spyOn(globalThis.crypto, 'getRandomValues')
        .mockImplementation(array => {
          (array as Uint32Array).fill(word);
          return array;
        });
      const unit = secureRandomUnit();
      expect(spy).toHaveBeenCalledTimes(1);
      expect(unit).toBeGreaterThanOrEqual(0);
      expect(unit).toBeLessThan(1);
      expect(unit).toBe(word / 0x1_0000_0000);
    },
  );
});

describe('generatePin', () => {
  test('draws its digits from the platform source', () => {
    const spy = jest.spyOn(globalThis.crypto, 'getRandomValues');
    const pin = generatePin();
    expect(spy).toHaveBeenCalled();
    expect(pin).toHaveLength(GENERATED_PIN_LENGTH);
    expect(isValidPin(pin)).toBe(true);
  });
});

describe.each([
  ['no crypto object', undefined],
  ['a crypto object without getRandomValues', {}],
])('on a phone with %s', (_, platform) => {
  let saved: PropertyDescriptor | undefined;
  beforeEach(() => {
    saved = Object.getOwnPropertyDescriptor(globalThis, 'crypto');
    Object.defineProperty(globalThis, 'crypto', {
      configurable: true,
      writable: true,
      value: platform,
    });
  });
  afterEach(() => {
    if (saved) {
      Object.defineProperty(globalThis, 'crypto', saved);
    } else {
      delete (globalThis as {crypto?: unknown}).crypto;
    }
  });

  test('secureRandomBytes refuses', () => {
    expect(() => secureRandomBytes(8)).toThrow('no platform random source');
  });

  test('secureRandomUnit refuses', () => {
    expect(() => secureRandomUnit()).toThrow('no platform random source');
  });

  test('generatePin refuses rather than draw from a weaker source', () => {
    expect(() => generatePin()).toThrow('no platform random source');
  });
});

describe('app sources', () => {
  const APP = path.resolve(__dirname, '..');

  /** What Metro can bundle: the entry, the root component and all of src. */
  function sources(): string[] {
    const found = ['index.js', 'App.tsx'];
    const walk = (dir: string): void => {
      const entries = fs.readdirSync(path.join(APP, dir), {
        withFileTypes: true,
      });
      for (const entry of entries) {
        const file = `${dir}/${entry.name}`;
        if (entry.isDirectory()) {
          walk(file);
        } else if (/\.(js|jsx|ts|tsx)$/.test(entry.name)) {
          found.push(file);
        }
      }
    };
    walk('src');
    return found;
  }

  test("no app source draws from the engine's non-cryptographic random", () => {
    const all = sources();
    expect(all).toEqual(
      expect.arrayContaining([
        'index.js',
        'App.tsx',
        'src/pin.ts',
        'src/screens/LoopScreen.tsx',
        'src/screens/ResetScreen.tsx',
      ]),
    );
    const weak = /\bMath\s*\.\s*random\b/;
    const sites = all.flatMap(file =>
      fs
        .readFileSync(path.join(APP, file), 'utf8')
        .split('\n')
        .flatMap((line, i) => (weak.test(line) ? [`${file}:${i + 1}`] : [])),
    );
    expect(sites).toEqual([]);
  });

  test.each(['src/screens/LoopScreen.tsx', 'src/screens/ResetScreen.tsx'])(
    '%s hands cardkit the platform source for the host challenge',
    file => {
      const options = fs
        .readFileSync(path.join(APP, file), 'utf8')
        .split('\n')
        .filter(line => /^\s*random:/.test(line))
        .map(line => line.trim());
      expect(options).toEqual(['random: secureRandomBytes,']);
    },
  );
});
