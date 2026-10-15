import { test } from 'node:test';
import assert from 'node:assert/strict';
import { termsAccepted } from './consent.ts';

test('only a checked checkbox counts as acceptance', () => {
  assert.equal(termsAccepted('on'), true);
  assert.equal(termsAccepted(null), false);
  assert.equal(termsAccepted(''), false);
  assert.equal(termsAccepted('off'), false);
});
