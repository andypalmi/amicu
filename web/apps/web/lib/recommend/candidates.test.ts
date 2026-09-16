// Pure-logic tests for the guided-wizard candidate helpers
// (andypalmi/amicu-planning#31/#35). Uses Node's built-in test runner
// (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  archetypeLabel,
  looksLikeCandidatesResponse,
  looksLikeCustomizeResponse,
  looksLikeQuestions,
} from './candidates.ts';

test('archetypeLabel gives a distinct human label per archetype', () => {
  const archetypes = [
    'simplest',
    'balanced',
    'cost_optimized',
    'performance_optimized',
    'local_development',
  ] as const;
  const labels = archetypes.map(archetypeLabel);
  assert.equal(new Set(labels).size, archetypes.length);
  assert.equal(archetypeLabel('cost_optimized'), 'Cost-optimized');
});

test('looksLikeQuestions accepts a QuestionDto[] and rejects malformed bodies', () => {
  assert.equal(
    looksLikeQuestions([
      { id: 'context_length', prompt: 'How much context?', kind: { type: 'number' }, required: true, hard: false },
    ]),
    true,
  );
  assert.equal(looksLikeQuestions([]), true);
  assert.equal(looksLikeQuestions(null), false);
  assert.equal(looksLikeQuestions({ error: 'nope' }), false);
  assert.equal(looksLikeQuestions([{ id: 'x' }]), false);
});

test('looksLikeCandidatesResponse accepts a candidates array and rejects malformed bodies', () => {
  assert.equal(looksLikeCandidatesResponse({ candidates: [] }), true);
  assert.equal(looksLikeCandidatesResponse({ candidates: [{}] }), true);
  assert.equal(looksLikeCandidatesResponse(null), false);
  assert.equal(looksLikeCandidatesResponse({}), false);
  assert.equal(looksLikeCandidatesResponse({ candidates: 'no' }), false);
});

test('looksLikeCustomizeResponse accepts a valid shape and rejects malformed bodies', () => {
  assert.equal(
    looksLikeCustomizeResponse({ hcl: 'resource "model" "m" {}', report: { valid: true, diagnostics: [] } }),
    true,
  );
  // 422 shape - plan/graph absent, still a valid CustomizeResponseDto.
  assert.equal(
    looksLikeCustomizeResponse({ hcl: 'raw', report: { valid: false, diagnostics: [] } }),
    true,
  );
  assert.equal(looksLikeCustomizeResponse(null), false);
  assert.equal(looksLikeCustomizeResponse({ hcl: 'x' }), false);
  assert.equal(looksLikeCustomizeResponse({ report: {} }), false);
});
