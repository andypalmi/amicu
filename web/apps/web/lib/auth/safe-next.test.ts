import { test } from 'node:test';
import assert from 'node:assert/strict';
import { safeNextPath } from './safe-next.ts';

const FB = 'https://app.amicu.dev/';

test('accepts local absolute paths', () => {
  for (const p of ['/configs', '/configs/abc?x=1#y', '/app', '/new/guided'])
    assert.equal(safeNextPath(p, FB), p);
});

test('rejects open-redirect shapes', () => {
  for (const p of [
    '//evil.com',
    '/\\evil.com',
    '/\\/evil.com',
    '\\evil.com',
    'https://evil.com',
    'javascript:alert(1)',
    '',
    'configs',
    '/\tfoo',
    '/\n//evil.com',
    '/foo\r\nSet-Cookie: x',
    '/a\u0000b',
  ])
    assert.equal(safeNextPath(p, FB), FB, JSON.stringify(p));
  assert.equal(safeNextPath(null, FB), FB);
  assert.equal(safeNextPath(undefined, FB), FB);
});
