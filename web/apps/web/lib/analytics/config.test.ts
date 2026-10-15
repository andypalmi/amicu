import { test } from 'node:test';
import assert from 'node:assert/strict';
import { cfBeaconToken, pageviewUrl, posthogKey, posthogOptions, privacyOptOut, sanitizeCapture } from './config.ts';

test('unset or blank env is a no-op', () => {
  for (const v of [undefined, '', '  ']) {
    assert.equal(cfBeaconToken(v), null);
    assert.equal(posthogKey(v), null);
  }
  assert.equal(posthogKey(' phc_x '), 'phc_x');
});

test('DNT and GPC opt out', () => {
  assert.equal(privacyOptOut({}), false);
  assert.equal(privacyOptOut({ doNotTrack: '0', globalPrivacyControl: false }), false);
  assert.equal(privacyOptOut({ doNotTrack: '1' }), true);
  assert.equal(privacyOptOut({ doNotTrack: 'yes' }), true);
  assert.equal(privacyOptOut({ globalPrivacyControl: true }), true);
});

test('posthog options are cookieless and minimal', () => {
  const o = posthogOptions();
  assert.equal(o.api_host, 'https://eu.i.posthog.com');
  assert.equal(o.persistence, 'memory');
  assert.equal(o.disable_session_recording, true);
  assert.equal(o.autocapture, false);
  assert.equal(o.respect_dnt, true);
  assert.equal(o.person_profiles, 'identified_only');
});

test('pageview url has no query', () => {
  assert.equal(pageviewUrl('https://amicu.dev', '/verify'), 'https://amicu.dev/verify');
});

test("sanitizeCapture strips query and fragment from all url props", () => {
  const cr = sanitizeCapture({
    event: "$pageview",
    properties: {
      $current_url: "https://amicu.dev/verify?token=s",
      $referrer: "https://x.dev/a?b=c#d",
      $initial_referrer: "$direct",
      $initial_current_url: "https://amicu.dev/?utm=1",
      n: 3,
    },
  });
  assert.equal(cr.properties.$current_url, "https://amicu.dev/verify");
  assert.equal(cr.properties.$referrer, "https://x.dev/a");
  assert.equal(cr.properties.$initial_referrer, "$direct");
  assert.equal(cr.properties.$initial_current_url, "https://amicu.dev/");
  assert.equal(sanitizeCapture(null), null);
});
