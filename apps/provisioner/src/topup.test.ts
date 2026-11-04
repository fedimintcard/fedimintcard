import {isValidTopup, MAX_TOPUP_NOTES, topupNotesMsat} from './settings';
import {planPreview} from './store';

describe('topupNotesMsat', () => {
  it('is empty for zero', () => {
    expect(topupNotesMsat(0)).toEqual([]);
  });
  it('splits the msat amount into powers of two, largest first', () => {
    // 2,000 sat = 2,000,000 msat = 2^20 + 2^19 + 2^18 + 2^17 + 2^15 + 2^10 + 2^7
    expect(topupNotesMsat(2000)).toEqual([
      1048576, 524288, 262144, 131072, 32768, 1024, 128,
    ]);
    expect(topupNotesMsat(2000).reduce((a, b) => a + b, 0)).toBe(2_000_000);
  });
});

describe('isValidTopup', () => {
  it('accepts zero (install only) and the demo sum', () => {
    expect(isValidTopup(0)).toBe(true);
    expect(isValidTopup(2000)).toBe(true);
  });
  it('refuses fractions, negatives, and sums that need more than the slots', () => {
    expect(isValidTopup(-1)).toBe(false);
    expect(isValidTopup(1.5)).toBe(false);
    expect(isValidTopup(NaN)).toBe(false);
    // 1,023 sat = 1,023,000 msat has 10 one-bits: more than 8 notes.
    expect(topupNotesMsat(1023).length).toBeGreaterThan(MAX_TOPUP_NOTES);
    expect(isValidTopup(1023)).toBe(false);
  });
});

describe('planPreview', () => {
  it('is install-only for a null plan', () => {
    const p = planPreview({count: 3, plan: null});
    expect(p.ok).toBe(true);
    expect(p.installOnly).toBe(true);
    expect(p.lines).toEqual([]);
    expect(p.batchTotal).toBe(0);
  });
  it('lists the notes the sidecar will make and sums the batch', () => {
    const p = planPreview({count: 3, plan: {amounts: [2000]}});
    expect(p.ok).toBe(true);
    expect(p.installOnly).toBe(false);
    expect(p.lines).toHaveLength(7);
    expect(p.lines[0]).toBe('1,048.576 sat');
    expect(p.perCard).toBe(2000);
    expect(p.batchTotal).toBe(6000);
  });
  it('refuses a sum that splits into more notes than the card holds', () => {
    const p = planPreview({count: 1, plan: {amounts: [1023]}});
    expect(p.ok).toBe(false);
    expect(p.problem).toMatch(/more than the card's 8 slots/);
  });
});
