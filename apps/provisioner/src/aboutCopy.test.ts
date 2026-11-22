/**
 * Settings > About shows `EXPERIMENTAL_WARNING` to every operator and to store
 * review. Build 13 shipped a sentence from before the daily limit and the
 * top-up existed: it said the app loads value onto a card and that the card has
 * no spending limits. This test pins what the text may say about each.
 */
import {DEFAULT_DAILY_LIMIT_SAT} from './cardSetup';
import {EXPERIMENTAL_WARNING} from './settings';

describe('the About text', () => {
  it('says the app is experimental', () => {
    expect(EXPERIMENTAL_WARNING).toMatch(/^Experimental software\./);
  });

  it('says what the app does to a card: the software, the PIN and the limit', () => {
    expect(EXPERIMENTAL_WARNING).toMatch(/installs card software/);
    expect(EXPERIMENTAL_WARNING).toMatch(/PIN/);
    expect(EXPERIMENTAL_WARNING).toMatch(/daily spending limit/);
  });

  it('agrees with the batch form, which offers a daily limit', () => {
    expect(typeof DEFAULT_DAILY_LIMIT_SAT).toBe('number');
    expect(EXPERIMENTAL_WARNING).not.toMatch(/no spending limits?/i);
  });

  it('says cards leave the app empty', () => {
    expect(EXPERIMENTAL_WARNING).not.toMatch(/loads value/i);
    expect(EXPERIMENTAL_WARNING).toMatch(/leave this app empty/);
  });
});
