// Pure-logic tests for the tier model behind <TierGate>. Uses Node's built-in
// test runner (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/auth test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { isTier, tierRank, tierSatisfies, TIERS, TIER_LABELS } from './tier.ts';

test('every tier has a label', () => {
  for (const t of TIERS) {
    assert.equal(typeof TIER_LABELS[t], 'string');
  }
});

test('isTier accepts known tiers and rejects junk', () => {
  assert.ok(isTier('community'));
  assert.ok(isTier('hosted'));
  assert.equal(isTier('Community'), false); // case-sensitive: claim is lowercase
  assert.equal(isTier('free'), false);
  assert.equal(isTier(undefined), false);
  assert.equal(isTier(2), false);
});

test('ladder: satisfies when current rank >= required rank', () => {
  assert.ok(tierSatisfies('pro', 'community'));
  assert.ok(tierSatisfies('pro', 'pro'));
  assert.equal(tierSatisfies('community', 'pro'), false);
  assert.equal(tierSatisfies('pro', 'enterprise'), false);
  assert.ok(tierSatisfies('enterprise', 'pro'));
});

test('allow-list: satisfies only on exact membership', () => {
  assert.ok(tierSatisfies('pro', ['pro', 'enterprise']));
  assert.ok(tierSatisfies('hosted', ['hosted']));
  assert.equal(tierSatisfies('community', ['pro', 'enterprise']), false);
  // enterprise is NOT in the list even though it out-ranks pro - exact membership.
  assert.equal(tierSatisfies('enterprise', ['pro']), false);
});

test('rank ordering is community < pro < enterprise < hosted', () => {
  assert.ok(tierRank('community') < tierRank('pro'));
  assert.ok(tierRank('pro') < tierRank('enterprise'));
  assert.ok(tierRank('enterprise') < tierRank('hosted'));
});
