import {DEFAULT_DAILY_LIMIT_SAT} from './cardSetup';
import {dailyLimitOf} from './store';

test('a batch with no limit set has none (0); a village batch keeps the one it sets', () => {
  const base = {federation: 'f', count: 1, pinPolicy: 'batch'} as const;
  expect(DEFAULT_DAILY_LIMIT_SAT).toBe(0);
  expect(dailyLimitOf(base)).toBe(0);
  expect(dailyLimitOf({...base, dailyLimitSat: 0})).toBe(0);
  expect(dailyLimitOf({...base, dailyLimitSat: 25_000})).toBe(25_000);
});
