/**
 * The anchor the tests send: mainnet block 900 000 (June 2025, bits
 * 0x17023774), the header the applet's own tests carry (Headers.H900000), and
 * a phone clock an hour after it, so the header is inside the window.
 */
import type {OfflineSetup} from '../cardSetup';

export const H900000_HEX =
  '00a0ab20247d4d9f582f9750344cdf62c46d81d046be960340960100000000000000000070f96945530651135839d8adc3f40e595118ec74c7ad81a3d17bb022e554fb0c937f4268743702177ad05f92';
export const H900000: readonly number[] = H900000_HEX.match(/../g)!.map(b => parseInt(b, 16));
export const T900000 = 1749188499;
export const NOW_MS = (T900000 + 3600) * 1000;

export const TEST_REGISTRY_KEY: readonly number[] = [0x04, ...Array.from({length: 64}, (_, i) => (i * 5 + 3) & 0xff)];

/** A whole OfflineSetup for a test: no limit, a certificate of the right shape, the anchor above. */
export function testOffline(over: Partial<OfflineSetup> = {}): OfflineSetup {
  return {
    limitMsat: 0,
    registryKey: TEST_REGISTRY_KEY,
    certify: async (attestPub, slot0) => [0x01, ...slot0, ...attestPub.slice(0, 16)],
    header: H900000,
    now: () => NOW_MS,
    ...over,
  };
}
