import { test } from 'node:test';
import assert from 'node:assert/strict';
import { parseSigninEnabled } from './features.ts';

test('sign-in flag is off unless exactly "true"', () => {
  assert.equal(parseSigninEnabled(undefined), false);
  assert.equal(parseSigninEnabled(''), false);
  assert.equal(parseSigninEnabled('false'), false);
  assert.equal(parseSigninEnabled('1'), false);
  assert.equal(parseSigninEnabled('TRUE'), false);
  assert.equal(parseSigninEnabled(' true '), false);
  assert.equal(parseSigninEnabled('true'), true);
});
