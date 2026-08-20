// Pure-logic tests for the billing formatters. Uses Node's built-in test runner
// (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  planLabel,
  isTierDenied,
  nextAction,
  looksLikeBillingStatus,
  looksLikeRedirect,
  type BillingStatus,
} from './format.ts';

test('planLabel maps known tiers and falls back to the raw value', () => {
  assert.equal(planLabel('community'), 'Community');
  assert.equal(planLabel('pro'), 'Pro');
  assert.equal(planLabel('enterprise'), 'Enterprise');
  assert.equal(planLabel('hosted'), 'Hosted');
  assert.equal(planLabel('mystery'), 'mystery');
});

test('isTierDenied is true only for 403', () => {
  assert.equal(isTierDenied(403), true);
  assert.equal(isTierDenied(401), false);
  assert.equal(isTierDenied(200), false);
});

test('nextAction is unavailable when Stripe is not configured', () => {
  const status: BillingStatus = { configured: false, plan: 'community', has_subscription: false };
  assert.equal(nextAction(status), 'unavailable');
});

test('nextAction offers upgrade when configured without a subscription', () => {
  const status: BillingStatus = { configured: true, plan: 'community', has_subscription: false };
  assert.equal(nextAction(status), 'upgrade');
});

test('nextAction offers manage when a subscription exists', () => {
  const status: BillingStatus = { configured: true, plan: 'pro', has_subscription: true };
  assert.equal(nextAction(status), 'manage');
});

test('looksLikeBillingStatus accepts a well-formed status', () => {
  assert.equal(
    looksLikeBillingStatus({ configured: true, plan: 'pro', has_subscription: true }),
    true,
  );
});

test('looksLikeBillingStatus rejects malformed bodies', () => {
  assert.equal(looksLikeBillingStatus(null), false);
  assert.equal(looksLikeBillingStatus({ configured: 'yes', plan: 'pro', has_subscription: true }), false);
  assert.equal(looksLikeBillingStatus({ plan: 'pro', has_subscription: true }), false);
  assert.equal(looksLikeBillingStatus({ configured: true, plan: 1, has_subscription: true }), false);
});

test('looksLikeRedirect requires a non-empty url string', () => {
  assert.equal(looksLikeRedirect({ url: 'https://checkout.stripe.com/x' }), true);
  assert.equal(looksLikeRedirect({ url: '' }), false);
  assert.equal(looksLikeRedirect({}), false);
  assert.equal(looksLikeRedirect(null), false);
});
