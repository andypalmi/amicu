// Pure-logic tests for the guided-wizard answer-building helper.
// Uses Node's built-in test runner
// (`node --test`, with type-stripping) so no test framework is added.
// Run: `pnpm --filter @amicu/web test`.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { buildAnswers, type AnswerValues } from './answers.ts';
import type { QuestionDto } from './candidates.ts';

function q(overrides: Partial<QuestionDto> & Pick<QuestionDto, 'id' | 'kind'>): QuestionDto {
  return {
    prompt: overrides.id,
    required: false,
    hard: false,
    help: null,
    ...overrides,
  };
}

test('buildAnswers coerces a number question to a JS number', () => {
  const questions = [q({ id: 'context_length', kind: { type: 'number' } })];
  const result = buildAnswers(questions, { context_length: '4096' });
  assert.deepEqual(result, { ok: true, answers: { context_length: 4096 } });
});

test('buildAnswers coerces a boolean question to a JS boolean', () => {
  const questions = [q({ id: 'eu_residency_required', kind: { type: 'boolean' } })];
  assert.deepEqual(buildAnswers(questions, { eu_residency_required: true }), {
    ok: true,
    answers: { eu_residency_required: true },
  });
  // `false` is a real answer, not a blank - it must still be sent.
  assert.deepEqual(buildAnswers(questions, { eu_residency_required: false }), {
    ok: true,
    answers: { eu_residency_required: false },
  });
});

test('buildAnswers combines a money question amount and user-chosen currency', () => {
  const questions = [q({ id: 'budget', kind: { type: 'money' } })];
  const values: AnswerValues = { budget: { amount: '250', currency: 'EUR' } };
  assert.deepEqual(buildAnswers(questions, values), {
    ok: true,
    answers: { budget: { amount: 250, currency: 'EUR' } },
  });
});

test('buildAnswers passes text/choice questions through as the raw string', () => {
  const questions = [q({ id: 'priority', kind: { type: 'choice', options: ['cost', 'latency'] } })];
  assert.deepEqual(buildAnswers(questions, { priority: 'latency' }), {
    ok: true,
    answers: { priority: 'latency' },
  });
});

test('buildAnswers skips a blank optional field entirely', () => {
  const questions = [
    q({ id: 'context_length', kind: { type: 'number' }, required: true }),
    q({ id: 'max_latency_ms', kind: { type: 'number' }, required: false }),
  ];
  const result = buildAnswers(questions, { context_length: '2048', max_latency_ms: '' });
  assert.equal(result.ok, true);
  assert.ok(result.ok && !('max_latency_ms' in result.answers));
  assert.deepEqual(result.ok && result.answers, { context_length: 2048 });
});

test('buildAnswers skips a blank optional money field entirely', () => {
  const questions = [q({ id: 'budget', kind: { type: 'money' }, required: false })];
  const result = buildAnswers(questions, { budget: { amount: '', currency: 'USD' } });
  assert.deepEqual(result, { ok: true, answers: {} });
});

test('buildAnswers rejects a blank required field with a readable message', () => {
  const questions = [
    q({ id: 'context_length', prompt: 'Context length', kind: { type: 'number' }, required: true }),
  ];
  const result = buildAnswers(questions, {});
  assert.deepEqual(result, { ok: false, message: 'Context length is required.' });
});

test('buildAnswers lists every missing required field', () => {
  const questions = [
    q({ id: 'context_length', prompt: 'Context length', kind: { type: 'number' }, required: true }),
    q({ id: 'concurrency', prompt: 'Concurrency', kind: { type: 'number' }, required: true }),
  ];
  const result = buildAnswers(questions, {});
  assert.deepEqual(result, {
    ok: false,
    message: 'Context length, Concurrency are required.',
  });
});
