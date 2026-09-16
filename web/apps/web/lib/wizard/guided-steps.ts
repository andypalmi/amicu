// Pure, framework-free step-transition logic for the guided wizard
// (andypalmi/amicu-planning#85, `/new/guided`). Kept free of React so the
// transitions are unit-testable in isolation (see guided-steps.test.ts).
//
// `GuidedWizard` (components/wizard/guided/guided-wizard.tsx) drives a
// `useReducer` with this module as the single source of truth for its whole
// step-scoped state - not just the small `step`/`preset`/`candidate` slice
// mirrored into the URL, but also the heavier per-step data (submitted
// answers, ranked candidates, the customize hand-off), so that "going back a
// step resets everything downstream" is one pure, testable operation instead
// of several `setState` calls that could drift out of sync (mirrors the
// existing `pickPreset` pattern in the #31 exercise route's
// candidate-picker.tsx, generalized to every step).

import type { CandidateDto } from '@/lib/recommend/candidates';

/** Step 0: pick a preset. Step 4: confirm/create. */
export const FIRST_STEP = 0;
export const LAST_STEP = 4;

export type GuidedState = {
  step: number;
  presetId: string | null;
  /** The answers submitted to rank candidates, kept for reference/back-nav. */
  answers: Record<string, unknown> | null;
  /** The ranked candidates returned for `answers`. */
  candidates: CandidateDto[] | null;
  candidateIndex: number | null;
  /** The customize step's confirmed hand-off (the HCL the review step's plan
   *  was confirmed against), consumed by the confirm/create step's
   *  <SaveConfigForm>. */
  confirmedHcl: string | null;
};

export const INITIAL_GUIDED_STATE: GuidedState = {
  step: FIRST_STEP,
  presetId: null,
  answers: null,
  candidates: null,
  candidateIndex: null,
  confirmedHcl: null,
};

export type GuidedAction =
  | { type: 'select-preset'; presetId: string }
  | { type: 'submit-answers'; answers: Record<string, unknown>; candidates: CandidateDto[] }
  | { type: 'select-candidate'; index: number }
  | { type: 'confirm-review'; hcl: string }
  | { type: 'back' }
  | { type: 'go-to-step'; step: number };

/** Clamp a step to the valid `[FIRST_STEP, LAST_STEP]` range. */
export function clampStep(step: number): number {
  if (!Number.isFinite(step)) return FIRST_STEP;
  return Math.min(Math.max(step, FIRST_STEP), LAST_STEP);
}

/** Move to `step`, clearing every field that belongs to a later step than the
 *  one being moved to - so jumping (or going back) to step N never leaves
 *  stale data from a step > N behind for a later re-visit to see. */
function resetFromStep(state: GuidedState, step: number): GuidedState {
  const clamped = clampStep(step);
  return {
    step: clamped,
    presetId: clamped >= 1 ? state.presetId : null,
    answers: clamped >= 2 ? state.answers : null,
    candidates: clamped >= 2 ? state.candidates : null,
    candidateIndex: clamped >= 3 ? state.candidateIndex : null,
    confirmedHcl: clamped >= 4 ? state.confirmedHcl : null,
  };
}

export function guidedReducer(state: GuidedState, action: GuidedAction): GuidedState {
  switch (action.type) {
    case 'select-preset':
      return { ...INITIAL_GUIDED_STATE, step: 1, presetId: action.presetId };
    case 'submit-answers':
      return {
        ...resetFromStep(state, 2),
        answers: action.answers,
        candidates: action.candidates,
      };
    case 'select-candidate':
      return { ...resetFromStep(state, 3), candidateIndex: action.index };
    case 'confirm-review':
      return { ...resetFromStep(state, 4), confirmedHcl: action.hcl };
    case 'back':
      return resetFromStep(state, state.step - 1);
    case 'go-to-step':
      return resetFromStep(state, action.step);
    default:
      return state;
  }
}
