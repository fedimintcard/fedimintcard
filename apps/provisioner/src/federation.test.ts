import {isInviteCode, normalizeInvite} from './federation';

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
