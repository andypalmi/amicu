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

test('trailing slashes and encoded paths are matched', () => {
  assert.equal(r('amicu.dev', '/login/').kind, 'redirect');
  assert.deepEqual(r('app.amicu.dev', '/app/'), {
    kind: 'redirect',
    url: 'https://app.amicu.dev/',
    status: 307,
  });
  assert.deepEqual(r('amicu.dev', '/%6Cogin', '?a=1'), {
    kind: 'redirect',
    url: 'https://app.amicu.dev/%6Cogin?a=1',
    status: 301,
  });
  assert.equal(r('app.amicu.dev', '/%70ricing').kind, 'redirect');
  assert.equal(r('app.amicu.dev', '/pricing/').kind, 'redirect');
  // malformed escape: never throws, falls through
  assert.deepEqual(r('amicu.dev', '/%E0%A4%A'), { kind: 'next' });
});

test('prefix collisions are not treated as app or site paths', () => {
  for (const p of ['/apple-icon', '/newsletter', '/organization', '/applications', '/configsx'])
    assert.deepEqual(r('amicu.dev', p), { kind: 'next' }, p);
  for (const p of ['/docsify', '/pricing-faq', '/security.txt'])
    assert.deepEqual(r('app.amicu.dev', p), { kind: 'next' }, p);
});

test('host normalisation: ports, case and trailing dot', () => {
  assert.equal(classifyHost('amicu.dev.', SITE, APP), 'site');
  assert.equal(classifyHost('APP.amicu.dev.:443', SITE, APP), 'app');
  assert.equal(classifyHost('amicu.dev:8443', SITE, APP), 'site');
  assert.equal(r('amicu.dev.', '/login').kind, 'redirect');
  const L = ['http://localhost:3000', 'http://localhost:3001'] as const;
  assert.equal(classifyHost('localhost:3000', L[0], L[1]), 'site');
  assert.equal(classifyHost('localhost:3001', L[0], L[1]), 'app');
  assert.equal(classifyHost('localhost:4000', L[0], L[1]), 'other');
  assert.equal(classifyHost('evil-amicu.dev', SITE, APP), 'other');
  assert.equal(classifyHost('app.amicu.dev.evil.com', SITE, APP), 'other');
});

test('proxy classifies on the supplied host only (spoofed forwarded host is not an input)', () => {
  // resolveHostRoute has no forwarded-host parameter: a request for amicu.dev
  // stays a site request regardless of any x-forwarded-host header value.
  assert.deepEqual(r('amicu.dev', '/pricing'), { kind: 'next' });
  assert.deepEqual(r('app.amicu.dev', '/login'), { kind: 'next' });
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
