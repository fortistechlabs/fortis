import { test } from 'node:test';
import assert from 'node:assert/strict';
import { LN_ADDRESS_DOMAIN, normalizeName, checkName, parseAvailability } from './lnname.js';

test('names are trimmed and lower-cased before they are checked', () => {
  assert.equal(normalizeName('  DoNal '), 'donal');
  assert.equal(LN_ADDRESS_DOMAIN, 'fortis.cash');
});

test('the rules and their order match the server', () => {
  assert.equal(checkName('donal'), null);
  assert.equal(checkName('a.b-c_d9'), null);
  assert.equal(checkName('Donal'), 'characters');
  assert.equal(checkName('do nal'), 'characters');
  assert.equal(checkName('dönal'), 'characters');
  assert.equal(checkName('ab'), 'length');
  assert.equal(checkName('a'.repeat(33)), 'length');
  assert.equal(checkName('a'.repeat(32)), null);
  assert.equal(checkName('.abc'), 'edges');
  assert.equal(checkName('abc-'), 'edges');
  assert.equal(checkName('a..bc'), 'double_dot');
  assert.equal(checkName('admin'), 'reserved');
  assert.equal(checkName('xbt'), 'reserved');
  // Characters are checked before length: a short bad name says characters.
  assert.equal(checkName('A'), 'characters');
});

test('availability comes from the public lookup', () => {
  assert.equal(parseAvailability(200, '{"tag":"payRequest","callback":"x"}'), 'taken');
  assert.equal(parseAvailability(200, '{"status":"ERROR","reason":"no such address"}'), 'available');
  assert.equal(parseAvailability(500, '{"status":"ERROR"}'), 'unknown');
  assert.equal(parseAvailability(200, 'not json'), 'unknown');
  assert.equal(parseAvailability(404, '{}'), 'unknown');
});
