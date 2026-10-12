import { test } from 'node:test';
import assert from 'node:assert/strict';
import { company, resolveLegalPages } from './company.ts';

test('default config enables nothing', () => {
  assert.deepEqual(resolveLegalPages(company), {
    privacy: false,
    terms: false,
    imprint: false,
    contact: false,
  });
});

test('pages enable only when identity and their own content are filled', () => {
  const id = { ...company, legalName: 'X', address: 'a', country: 'IT', contactEmail: 'a@b.c' };
  assert.equal(resolveLegalPages(id).imprint, true);
  assert.equal(resolveLegalPages(id).privacy, false);
  const p = { ...id, privacyEffectiveDate: '2026-01-01', privacyBody: 'text' };
  assert.equal(resolveLegalPages(p).privacy, true);
  assert.equal(resolveLegalPages(p).terms, false);
});
