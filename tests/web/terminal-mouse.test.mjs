import { test } from 'node:test';
import assert from 'node:assert/strict';
import { withoutTracking, wheelReport, wheelTicks } from '../../crates/sm-server/src/web/terminal-mouse.js';

test('mouse tracking modes are taken out; other private modes stay', () => {
  assert.deepEqual(withoutTracking([1000]), { tracking: true, rest: [] });
  assert.deepEqual(withoutTracking([1049, 1002, 1006]), { tracking: true, rest: [1049, 1006] });
  assert.deepEqual(withoutTracking([2004]), { tracking: false, rest: [2004] });
  assert.deepEqual(withoutTracking([[1003]]), { tracking: true, rest: [] });
});

test('wheel reports are SGR at a 1-based cell', () => {
  assert.equal(wheelReport(true, 3, 7), '\x1b[<64;3;7M');
  assert.equal(wheelReport(false, 1, 1), '\x1b[<65;1;1M');
});

test('one wheel tick per event once a line has built up', () => {
  const carry = { value: 0 };
  assert.equal(wheelTicks({ deltaMode: 0, deltaY: 6 }, 18, carry), 0);
  assert.equal(wheelTicks({ deltaMode: 0, deltaY: 6 }, 18, carry), 0);
  assert.equal(wheelTicks({ deltaMode: 0, deltaY: 6 }, 18, carry), 1);
  assert.equal(wheelTicks({ deltaMode: 0, deltaY: -100 }, 18, carry), -1);
  assert.equal(wheelTicks({ deltaMode: 1, deltaY: 3 }, 18, { value: 0 }), 1);
});
