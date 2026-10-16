import { test } from 'node:test';
import assert from 'node:assert/strict';
import { FIAT_CODES, defaultFiat, parsePrices, fiatLabel, chosenFiat, setFiat, currentFiat } from './fiat.js';

test('every currency the edge sends is read', () => {
  assert.deepEqual(parsePrices({ USD: 655, EUR: 584.404, JPY: 103474.5717 }), { USD: 655, EUR: 584.404, JPY: 103474.5717 });
});

test('an older edge with USD only still works', () => {
  assert.deepEqual(parsePrices({ USD: 655 }), { USD: 655 });
});

test('junk is dropped, and no USD means no feed', () => {
  assert.deepEqual(parsePrices({ USD: 1, EUR: -1, GBP: 0, eur: 5, EURO: 5, time: 17, CHF: '9', JPY: null, SEK: Infinity }), { USD: 1 });
  assert.equal(parsePrices({ EUR: 584.4 }), null);
  assert.equal(parsePrices(null), null);
  assert.equal(parsePrices('nope'), null);
  assert.equal(parsePrices({ error: 'missing or invalid token' }), null);
});

test('the default follows the region', () => {
  assert.equal(defaultFiat('de-DE'), 'EUR');
  assert.equal(defaultFiat('en-IE'), 'EUR');
  assert.equal(defaultFiat('de'), 'EUR'); // likely region DE
  assert.equal(defaultFiat('en-GB'), 'GBP');
  assert.equal(defaultFiat('ja'), 'JPY');
  assert.equal(defaultFiat('en-US'), 'USD');
  assert.equal(defaultFiat('en'), 'USD');
  assert.equal(defaultFiat('en-NG'), 'USD'); // NGN isn't offered
  assert.equal(defaultFiat('not a tag!'), 'USD');
});

test('every offered code is a real currency, listed once', () => {
  assert.equal(new Set(FIAT_CODES).size, FIAT_CODES.length);
  for (const c of FIAT_CODES) assert.match(fiatLabel(c, 'en'), new RegExp(`^${c} · \\S`));
  assert.equal(fiatLabel('EUR', 'en'), 'EUR · Euro');
});

test('a choice holds even without storage, and only offered codes are taken', () => {
  // node has no localStorage: the page-only fallback is what's exercised here
  assert.equal(chosenFiat(), null);
  setFiat('GBP');
  assert.equal(chosenFiat(), 'GBP');
  assert.equal(currentFiat(), 'GBP');
  setFiat('XYZ');
  assert.equal(chosenFiat(), null);
  setFiat(null);
  assert.equal(chosenFiat(), null);
});
