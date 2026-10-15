// next/navigation cannot load under plain node, so the server-action gate is
// checked structurally: every exported function in the auth actions module must
// call assertSigninEnabled() (which notFound()s when sign-in is disabled) as its
// first statement, and no exported action may be added without it.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../../app/(auth)/actions.ts', import.meta.url), 'utf8');

test('every exported auth server action starts with assertSigninEnabled()', () => {
  const exported = [...src.matchAll(/^export async function (\w+)\(/gm)].map((m) => m[1]);
  assert.deepEqual(exported.sort(), [
    'forgotAction',
    'loginAction',
    'logoutAction',
    'resendVerificationAction',
    'signupAction',
    'verifyAction',
  ]);
  for (const name of exported) {
    const re = new RegExp(`export async function ${name}\\([^)]*\\)[^{]*\\{\\n  assertSigninEnabled\\(\\);`);
    assert.match(src, re, `${name} must be gated`);
  }
  assert.match(src, /if \(!SIGNIN_ENABLED\) notFound\(\);/);
});
