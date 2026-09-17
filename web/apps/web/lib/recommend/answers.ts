// Pure, framework-free answer-building logic for the guided wizard
// (andypalmi/amicu-planning#85), extracted from the #31/#35 exercise route's
// `rankCandidates` (see components/wizard/candidate-picker.tsx, since removed)
// so it can be unit-tested in isolation from React and reused by
// <QuestionForm>/<GuidedWizard>.
//
// Coerces the raw per-question form values `<QuestionForm>` collects into the
// `answers` object `POST /recommend/candidates` expects: numbers and booleans
// are cast from their string/boolean editing representation, money questions
// combine a user-chosen amount AND currency (fixing a review nit - the #31
// exercise route hardcoded `currency: 'USD''), blank optional fields are
// skipped entirely, and a blank required field is rejected with a
// human-readable message instead of being silently sent as `undefined`.

import type { QuestionDto } from './candidates';

/** A money question's raw editing value: an amount string plus its currency. */
export type MoneyAnswerValue = { amount: string; currency: string };

/** Raw per-question field value, keyed by `QuestionDto.id`, kept in the shape a
 *  form control edits (strings/booleans/money-pair) until `buildAnswers` casts
 *  it to what the API expects. */
export type AnswerValue = string | boolean | MoneyAnswerValue;
export type AnswerValues = Record<string, AnswerValue>;

export type BuildAnswersResult =
  | { ok: true; answers: Record<string, unknown> }
  | { ok: false; message: string };

function isBlank(value: AnswerValue | undefined): boolean {
  if (value === undefined) return true;
  if (typeof value === 'string') return value.trim() === '';
  if (typeof value === 'boolean') return false;
  return value.amount.trim() === '';
}

/**
 * Build the `answers` object `POST /recommend/candidates` expects from a
 * question list and the form's raw per-question values.
 *
 * - `number` questions are cast with `Number(...)`.
 * - `boolean` questions are cast with `Boolean(...)`.
 * - `money` questions combine `{ amount, currency }` into
 *   `{ amount: number, currency: string }`.
 * - Everything else (`text`, `choice`) is passed through as the raw string.
 * - A blank, non-required field is skipped entirely (not sent as `''` or
 *   `undefined`).
 * - A blank required field fails the whole build with a message naming every
 *   missing question, rather than silently omitting it.
 */
export function buildAnswers(
  questions: readonly QuestionDto[],
  values: AnswerValues,
): BuildAnswersResult {
  const answers: Record<string, unknown> = {};
  const missing: string[] = [];

  for (const q of questions) {
    const raw = values[q.id];
    if (isBlank(raw)) {
      if (q.required) missing.push(q.prompt);
      continue;
    }

    switch (q.kind.type) {
      case 'number':
        answers[q.id] = Number(raw);
        break;
      case 'boolean':
        answers[q.id] = Boolean(raw);
        break;
      case 'money': {
        const money = raw as MoneyAnswerValue;
        answers[q.id] = { amount: Number(money.amount), currency: money.currency };
        break;
      }
      default:
        answers[q.id] = raw;
    }
  }

  if (missing.length > 0) {
    return {
      ok: false,
      message: `${missing.join(', ')} ${missing.length === 1 ? 'is' : 'are'} required.`,
    };
  }

  return { ok: true, answers };
}
