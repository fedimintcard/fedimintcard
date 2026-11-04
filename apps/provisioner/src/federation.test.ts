import {
  decodeInvite,
  guardianLabel,
  isInviteCode,
  normalizeInvite,
  shortFederationId,
} from './federation';

describe('normalizeInvite', () => {
  it('takes the bech32m form, lowercased and trimmed', () => {
    expect(normalizeInvite('  FED11QGQZXGTHWDEN5TE0V9CXJTT4V9UX2WFJXVNQQ9U \n')).toBe(
      'fed11qgqzxgthwden5te0v9cxjtt4v9ux2wfjxvnqq9u',
    );
  });
  it('takes the fedimint-prefixed base32hex form', () => {
    expect(normalizeInvite('fedimint1h4s0a2b3c4d5e6f')).toBe(
      'fedimint1h4s0a2b3c4d5e6f',
    );
  });
  it('refuses what the sidecar would refuse', () => {
    expect(normalizeInvite('')).toBeNull();
    expect(normalizeInvite('fedimint://x')).toBeNull();
    expect(normalizeInvite('fed11qgq1')).toBeNull(); // "1" is not bech32
    expect(normalizeInvite('lnbc1...')).toBeNull();
    expect(isInviteCode('https://example.org')).toBe(false);
  });
});

// The invite an operator scanned from a guardian UI's "Connect a Client" QR
// on 2026-11-04: one guardian over iroh, and the federation id.
const SCANNED =
  'fed11qgqyj3mfwfhksw309uekyvesvenrzdpjx93xvdf4xcex2d33xajxycmzxf3nzdn9x9jxyctpxy6k2vnpv56nwvtpvsmxgepnx93kvvnzvscnjefn8qurxc3eqqqjp8wgs64nt3zuzc4j073a5mlr2cm4kp6t2jtv06d05kcu3elavy5g5h4kwl';

describe('decodeInvite', () => {
  it('reads the federation id and the guardian from a real invite', () => {
    const info = decodeInvite(SCANNED);
    expect(info).not.toBeNull();
    expect(info?.federationId).toBe(
      '9dc886ab35c45c162b27fa3da6fe356375b074b5496c7e9afa5b1c8e7fd61288',
    );
    expect(info?.guardians).toEqual([
      {
        peer: 0,
        url: 'iroh://3b30ff1421bf5562e617dbcb2c16e1dbaa15e2ae571ad6dd31cf2bd19e3883b9',
      },
    ]);
  });
  it('is null for text that is not an invite', () => {
    expect(decodeInvite('fed11qgqz')).toBeNull();
    expect(decodeInvite('hello')).toBeNull();
  });
  it('labels a federation and its guardians for one line', () => {
    expect(shortFederationId('9dc886ab35c4')).toBe('9dc8 86ab');
    expect(guardianLabel('iroh://3b30ff14')).toBe('iroh node 3b30…');
    expect(guardianLabel('wss://api.example.org/')).toBe('api.example.org');
  });
});
