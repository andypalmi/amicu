import { test } from 'node:test';
import assert from 'node:assert/strict';
import { company, resolveLegalPages } from './company.ts';

test('default config: privacy, terms and contact live; imprint hidden', () => {
  assert.deepEqual(resolveLegalPages(company), {
    privacy: true,
    terms: true,
    imprint: false,
    contact: true,
  });
});

test('default config publishes no personal identity', () => {
  assert.equal(company.controllerName, null);
  assert.equal(company.legalName, null);
  assert.equal(company.address, null);
});

test('pages need their own date and contact address', () => {
  assert.equal(resolveLegalPages({ ...company, privacyEffectiveDate: null }).privacy, false);
  assert.equal(resolveLegalPages({ ...company, privacyEmail: null }).privacy, false);
  assert.equal(resolveLegalPages({ ...company, termsEffectiveDate: null }).terms, false);
  assert.equal(resolveLegalPages({ ...company, contactEmail: null }).terms, false);
});

test('imprint enables only when identity is filled', () => {
  const id = { ...company, legalName: 'X', address: 'a', country: 'IT' };
  assert.equal(resolveLegalPages(id).imprint, true);
  assert.equal(resolveLegalPages({ ...id, address: null }).imprint, false);
});
