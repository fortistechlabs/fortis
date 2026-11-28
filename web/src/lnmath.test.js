import { test } from 'node:test';
import assert from 'node:assert/strict';
import { routingFeeCapMsat, offerReserveMsat, sendAllSat } from './lnmath.js';

test('the fee cap is 1 % and at least 1 sat', () => {
  assert.equal(routingFeeCapMsat(0), 1_000);
  assert.equal(routingFeeCapMsat(2_000), 1_000);
  assert.equal(routingFeeCapMsat(100_000), 1_000);
  assert.equal(routingFeeCapMsat(199_999), 1_999);
  assert.equal(routingFeeCapMsat(1_000_000), 10_000);
  assert.equal(routingFeeCapMsat(100_000_000), 1_000_000);
  assert.equal(offerReserveMsat(121_000), routingFeeCapMsat(121_000));
  assert.equal(offerReserveMsat(121_000), 1_210);
});

test('send all is the largest amount that fits with its own fee cap', () => {
  const balances = [];
  for (let b = 0; b <= 400_000; b += 7) balances.push(b);
  balances.push(101_000, 101_009, 101_010, 2_000, 1_999, 3_000, 2_500_000, 100_000_000_000);
  for (const b of balances) {
    const a = sendAllSat(b);
    if (a > 0) {
      const amount = a * 1000;
      assert.ok(amount + routingFeeCapMsat(amount) <= b, `fits: balance ${b} amount ${amount}`);
    }
    const more = (a + 1) * 1000;
    assert.ok(more + routingFeeCapMsat(more) > b, `one sat more does not fit: balance ${b}`);
  }
  assert.equal(sendAllSat(1_999), 0);
  assert.equal(sendAllSat(2_000), 1);
  assert.equal(sendAllSat(3_000), 2); // the 2 sat the user had
  assert.equal(sendAllSat(101_000), 100);
});
