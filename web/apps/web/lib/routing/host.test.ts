import { test } from 'node:test';
import assert from 'node:assert/strict';
import { classifyHost, resolveHostRoute } from './host.ts';

const SITE = 'https://amicu.dev';
const APP = 'https://app.amicu.dev';
const r = (host: string, pathname: string, search = '') =>
  resolveHostRoute({ host, pathname, search, siteOrigin: SITE, appOrigin: APP });

test('classifyHost', () => {
  assert.equal(classifyHost('app.amicu.dev', SITE, APP), 'app');
  assert.equal(classifyHost('AMICU.dev', SITE, APP), 'site');
  assert.equal(classifyHost('localhost:3000', SITE, APP), 'other');
  assert.equal(classifyHost('amicu.dev', SITE, null), 'other');
  assert.equal(classifyHost('amicu.dev', SITE, SITE), 'other');
});

test('app host: root rewrites to dashboard, /app redirects to clean root', () => {
  assert.deepEqual(r('app.amicu.dev', '/'), { kind: 'rewrite', pathname: '/app' });
  assert.deepEqual(r('app.amicu.dev', '/app', '?a=1'), {
    kind: 'redirect',
    url: 'https://app.amicu.dev/?a=1',
    status: 307,
  });
});

test('app host: app, auth and api routes pass through', () => {
  for (const p of ['/login', '/configs/abc', '/billing', '/api/engine/plan', '/_next/static/x.js', '/robots.txt'])
    assert.deepEqual(r('app.amicu.dev', p), { kind: 'next' }, p);
});

test('app host: marketing and docs 301 to the site, preserving path+query', () => {
  assert.deepEqual(r('app.amicu.dev', '/pricing'), {
    kind: 'redirect',
    url: 'https://amicu.dev/pricing',
    status: 301,
  });
  assert.deepEqual(r('app.amicu.dev', '/docs/cli/plan', '?x=1'), {
    kind: 'redirect',
    url: 'https://amicu.dev/docs/cli/plan?x=1',
    status: 301,
  });
  assert.equal(r('app.amicu.dev', '/sitemap.xml').kind, 'redirect');
  assert.equal(r('app.amicu.dev', '/privacy').kind, 'redirect');
});

test('site host: app and auth paths 301 to the app host', () => {
  assert.deepEqual(r('amicu.dev', '/login', '?next=%2Fconfigs'), {
    kind: 'redirect',
    url: 'https://app.amicu.dev/login?next=%2Fconfigs',
    status: 301,
  });
  assert.deepEqual(r('amicu.dev', '/configs/abc'), {
    kind: 'redirect',
    url: 'https://app.amicu.dev/configs/abc',
    status: 301,
  });
  assert.deepEqual(r('amicu.dev', '/app'), {
    kind: 'redirect',
    url: 'https://app.amicu.dev/',
    status: 301,
  });
  assert.equal(r('amicu.dev', '/verify', '?token=t').kind, 'redirect');
});

test('site host: marketing, docs and api are untouched', () => {
  for (const p of ['/', '/pricing', '/docs/x', '/api/search', '/api/engine/plan', '/application'])
    assert.deepEqual(r('amicu.dev', p), { kind: 'next' }, p);
});

test('other hosts (localhost, previews) never redirect', () => {
  for (const p of ['/', '/login', '/app', '/pricing', '/docs'])
    assert.deepEqual(r('localhost:3000', p), { kind: 'next' }, p);
});

test('trailing slash on configured origins is tolerated', () => {
  const a = resolveHostRoute({
    host: 'amicu.dev',
    pathname: '/login',
    siteOrigin: 'https://amicu.dev/',
    appOrigin: 'https://app.amicu.dev/',
  });
  assert.deepEqual(a, { kind: 'redirect', url: 'https://app.amicu.dev/login', status: 301 });
});
