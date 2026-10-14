// Pure-logic tests for the guided wizard's step reducer.
// Uses Node's built-in test runner
// (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  clampStep,
  guidedReducer,
  INITIAL_GUIDED_STATE,
  FIRST_STEP,
  LAST_STEP,
  type GuidedState,
} from './guided-steps.ts';
import type { CandidateDto } from '@/lib/recommend/candidates';

const fakeCandidates = [{ archetype: 'balanced' }, { archetype: 'simplest' }] as unknown as CandidateDto[];

test('clampStep never goes below the first step or past the last step', () => {
  assert.equal(clampStep(-5), FIRST_STEP);
  assert.equal(clampStep(-1), FIRST_STEP);
  assert.equal(clampStep(0), 0);
  assert.equal(clampStep(4), 4);
  assert.equal(clampStep(9), LAST_STEP);
  assert.equal(clampStep(Number.NaN), FIRST_STEP);
});

test('select-preset moves to step 1 and resets everything to a clean slate', () => {
  const dirty: GuidedState = {
    step: 4,
    presetId: 'old-preset',
    answers: { context_length: 4096 },
    candidates: fakeCandidates,
    candidateIndex: 1,
    confirmedHcl: 'resource "model" "m" {}',
  };
  const next = guidedReducer(dirty, { type: 'select-preset', presetId: 'new-preset' });
  assert.deepEqual(next, { ...INITIAL_GUIDED_STATE, step: 1, presetId: 'new-preset' });
});

test('submit-answers moves to step 2 carrying the answers/candidates, clearing later steps', () => {
  const state: GuidedState = {
    ...INITIAL_GUIDED_STATE,
    step: 1,
    presetId: 'p1',
  };
  const next = guidedReducer(state, {
    type: 'submit-answers',
    answers: { context_length: 2048 },
    candidates: fakeCandidates,
  });
  assert.equal(next.step, 2);
  assert.equal(next.presetId, 'p1');
  assert.deepEqual(next.answers, { context_length: 2048 });
  assert.equal(next.candidates, fakeCandidates);
  assert.equal(next.candidateIndex, null);
  assert.equal(next.confirmedHcl, null);
});

test('select-candidate moves to step 3, clearing any prior confirmed HCL', () => {
  const state: GuidedState = {
    step: 2,
    presetId: 'p1',
    answers: { context_length: 2048 },
    candidates: fakeCandidates,
    candidateIndex: null,
    confirmedHcl: 'stale',
  };
  const next = guidedReducer(state, { type: 'select-candidate', index: 1 });
  assert.equal(next.step, 3);
  assert.equal(next.candidateIndex, 1);
  assert.equal(next.confirmedHcl, null);
  // Earlier-step data is untouched.
  assert.equal(next.presetId, 'p1');
  assert.deepEqual(next.answers, { context_length: 2048 });
});

test('confirm-review moves to step 4 carrying the confirmed HCL', () => {
  const state: GuidedState = {
    step: 3,
    presetId: 'p1',
    answers: { context_length: 2048 },
    candidates: fakeCandidates,
    candidateIndex: 0,
    confirmedHcl: null,
  };
  const next = guidedReducer(state, { type: 'confirm-review', hcl: 'resource "model" "m" {}' });
  assert.equal(next.step, 4);
  assert.equal(next.confirmedHcl, 'resource "model" "m" {}');
});

test('go-to-step clamps past the last step and clears state beyond the clamped target', () => {
  const state: GuidedState = {
    step: 3,
    presetId: 'p1',
    answers: { context_length: 2048 },
    candidates: fakeCandidates,
    candidateIndex: 0,
    confirmedHcl: null,
  };
  const next = guidedReducer(state, { type: 'go-to-step', step: 99 });
  assert.equal(next.step, LAST_STEP);
  // Nothing to clear beyond step 4 (there isn't one), so data is preserved.
  assert.equal(next.candidateIndex, 0);
});

test('go-to-step resetting to an earlier step clears every field after it, keeping earlier ones', () => {
  const state: GuidedState = {
    step: 3,
    presetId: 'p1',
    answers: { context_length: 2048 },
    candidates: fakeCandidates,
    candidateIndex: 0,
    confirmedHcl: null,
  };
  const next = guidedReducer(state, { type: 'go-to-step', step: 1 });
  assert.deepEqual(next, {
    step: 1,
    presetId: 'p1',
    answers: null,
    candidates: null,
    candidateIndex: null,
    confirmedHcl: null,
  });
});

test('go-to-step to step 0 clears everything, including the preset', () => {
  const state: GuidedState = {
    step: 3,
    presetId: 'p1',
    answers: { context_length: 2048 },
    candidates: fakeCandidates,
    candidateIndex: 0,
    confirmedHcl: null,
  };
  const next = guidedReducer(state, { type: 'go-to-step', step: 0 });
  assert.deepEqual(next, INITIAL_GUIDED_STATE);
});
