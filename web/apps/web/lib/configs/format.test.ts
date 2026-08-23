// Pure-logic tests for the My Configs formatters and guards. Uses Node's
// built-in test runner (`node --test`, with type-stripping) so no test framework
// is added. Run: `node --test --experimental-strip-types 'lib/**/*.test.ts'`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  looksLikeConfigSummaryList,
  looksLikeConfigDetail,
  looksLikeVersionMetaList,
  looksLikeVersionMeta,
  looksLikeVersionContent,
  looksLikeConfigDiff,
  shortSha,
  apiErrorMessage,
  errorMessage,
  classifyDiffLine,
} from './format.ts';

test('looksLikeConfigSummaryList accepts a well-formed list, including empty', () => {
  assert.equal(
    looksLikeConfigSummaryList([
      { id: 'cfg_1', name: 'prod', current_version: 3, updated_at: 100 },
    ]),
    true,
  );
  assert.equal(looksLikeConfigSummaryList([]), true);
});

test('looksLikeConfigSummaryList rejects malformed bodies', () => {
  assert.equal(looksLikeConfigSummaryList(null), false);
  assert.equal(looksLikeConfigSummaryList({}), false);
  assert.equal(
    looksLikeConfigSummaryList([{ id: 1, name: 'x', current_version: 1, updated_at: 1 }]),
    false,
  );
  assert.equal(looksLikeConfigSummaryList([{ id: 'cfg_1', name: 'x' }]), false);
});

test('looksLikeConfigDetail validates the detail shape', () => {
  assert.equal(
    looksLikeConfigDetail({
      id: 'cfg_1',
      name: 'prod',
      content: 'runtime {}',
      content_sha256: 'abc',
      current_version: 2,
      updated_at: 100,
    }),
    true,
  );
  assert.equal(looksLikeConfigDetail({ id: 'cfg_1', name: 'prod' }), false);
  assert.equal(looksLikeConfigDetail(null), false);
});

test('looksLikeVersionMetaList / looksLikeVersionMeta validate version metadata', () => {
  const meta = { version_no: 1, created_at: 100, content_sha256: 'abc', message: null };
  assert.equal(looksLikeVersionMetaList([meta]), true);
  assert.equal(looksLikeVersionMetaList([]), true);
  assert.equal(looksLikeVersionMeta(meta), true);
  assert.equal(looksLikeVersionMeta({ version_no: '1' }), false);
  assert.equal(looksLikeVersionMetaList([{ version_no: 1 }]), false);
});

test('looksLikeVersionContent validates full version content', () => {
  assert.equal(
    looksLikeVersionContent({
      version_no: 1,
      created_at: 100,
      content: 'runtime {}',
      content_sha256: 'abc',
    }),
    true,
  );
  assert.equal(looksLikeVersionContent({ version_no: 1, created_at: 100 }), false);
});

test('looksLikeConfigDiff validates the diff shape', () => {
  assert.equal(looksLikeConfigDiff({ from: 1, to: 2, diff: '@@ -1 +1 @@' }), true);
  assert.equal(looksLikeConfigDiff({ from: '1', to: 2, diff: '' }), false);
  assert.equal(looksLikeConfigDiff(null), false);
});

test('shortSha truncates to a 10-character prefix', () => {
  assert.equal(shortSha('abcdef0123456789'), 'abcdef0123');
  assert.equal(shortSha('short'), 'short');
});

test('apiErrorMessage extracts the API error string, or null', () => {
  assert.equal(apiErrorMessage({ error: 'name already exists' }), 'name already exists');
  assert.equal(apiErrorMessage({ error: 42 }), null);
  assert.equal(apiErrorMessage(null), null);
  assert.equal(apiErrorMessage('nope'), null);
});

test('errorMessage maps known statuses and prefers the API message', () => {
  assert.equal(
    errorMessage(401, null),
    'Your session has expired. Reload the page or sign in again.',
  );
  assert.equal(errorMessage(400, { error: 'bad HCL' }), 'bad HCL');
  assert.equal(errorMessage(400, null), 'That request was malformed.');
  assert.equal(errorMessage(404, null), 'That configuration or version no longer exists.');
  assert.equal(errorMessage(409, { error: 'name taken' }), 'name taken');
  assert.equal(errorMessage(409, null), 'A configuration with that name already exists.');
  assert.equal(errorMessage(502, null), 'The configuration service is unreachable.');
  assert.equal(errorMessage(500, null), 'Request failed (500).');
});

test('classifyDiffLine tags unified-diff line kinds', () => {
  assert.equal(classifyDiffLine('+++ b/config.hcl'), 'header');
  assert.equal(classifyDiffLine('--- a/config.hcl'), 'header');
  assert.equal(classifyDiffLine('@@ -1,3 +1,3 @@'), 'hunk');
  assert.equal(classifyDiffLine('+added line'), 'add');
  assert.equal(classifyDiffLine('-removed line'), 'remove');
  assert.equal(classifyDiffLine(' unchanged line'), 'context');
});
