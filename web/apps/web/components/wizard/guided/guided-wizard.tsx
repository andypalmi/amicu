'use client';

import { useEffect, useReducer, useRef, useState } from 'react';
import { useRouter, useSearchParams } from 'next/navigation';
import { Alert, AlertDescription, Badge, Button, Card, CardContent } from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { LoadingState } from '@/components/common/loading-state';
import { Gallery } from '@/components/wizard/gallery';
import { QuestionField } from '@/components/wizard/question-form';
import { CandidateGrid } from '@/components/wizard/candidate-grid';
import { CandidateReview } from '@/components/wizard/candidate-review';
import { SaveConfigForm } from '@/components/configs/save-config-form';
import { PlanPanel } from '@/components/engine/plan-panel';
import { Stepper } from '@/components/wizard/stepper';
import {
  buildAnswers,
  type AnswerValue,
  type AnswerValues,
} from '@/lib/recommend/answers';
import {
  archetypeLabel,
  looksLikeCandidatesResponse,
  looksLikeQuestions,
  type CandidateDto,
  type QuestionDto,
} from '@/lib/recommend/candidates';
import { formatRangeMs, type PresetSummaryDto } from '@/lib/wizard/format';
import { guidedReducer, INITIAL_GUIDED_STATE, LAST_STEP } from '@/lib/wizard/guided-steps';
import type { Plan } from '@/lib/engine/format';

const STEP_LABELS = ['Preset', 'Questions', 'Candidates', 'Review', 'Confirm'] as const;

type QuestionsState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; questions: QuestionDto[] };

type CandidatesState =
  | { kind: 'idle' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; candidates: CandidateDto[] };

// The production guided wizard (andypalmi/amicu-planning#85): a deterministic,
// five-step stepper over the SAME BFF routes the #31/#35 exercise route used -
// pick a preset, answer its engine-provided core questions, rank candidates,
// review/edit one's HCL and its plan, then save it as a named config (the
// confirm/create step - there is no separate backend call for "creating" a
// config from the wizard, `POST /api/configs` IS the hand-off).
//
// All step-scoped state (preset, submitted answers, ranked candidates, the
// selected candidate, the confirmed HCL) lives in one `useReducer` -
// `guidedReducer` (lib/wizard/guided-steps.ts) is the single, pure, unit-tested
// place that decides what a step change clears. Only a small slice of it
// (`step`, `preset`, `candidate`) is mirrored into the URL for shareable
// back/forward; nothing is persisted across a hard refresh (same behavior as
// the classic `/wizard`) - see the mount effect below.
export function GuidedWizard() {
  const router = useRouter();
  const searchParams = useSearchParams();
  const [state, dispatch] = useReducer(guidedReducer, INITIAL_GUIDED_STATE);

  const [presetObj, setPresetObj] = useState<PresetSummaryDto | null>(null);
  const [questionsState, setQuestionsState] = useState<QuestionsState>({ kind: 'idle' });
  const [values, setValues] = useState<AnswerValues>({});
  const [answerError, setAnswerError] = useState<string | null>(null);
  const [ranking, setRanking] = useState(false);
  const [candidatesState, setCandidatesState] = useState<CandidatesState>({ kind: 'idle' });
  const [confirmedPlan, setConfirmedPlan] = useState<Plan | null>(null);

  // On mount, a URL that already points past step 0 (e.g. a hard refresh, or a
  // bookmarked link) can never be honored - the heavier state it depends on
  // (questions, ranked candidates, the customize result) is never persisted.
  // Reset the URL back to the start rather than rendering a step with no data
  // behind it, matching the classic `/wizard`'s no-persistence behavior.
  const didInit = useRef(false);
  useEffect(() => {
    if (didInit.current) return;
    didInit.current = true;
    const urlStep = Number(searchParams.get('step') ?? '0');
    if (Number.isFinite(urlStep) && urlStep > 0) {
      router.replace('/new/guided');
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Mirror the reducer's step/preset/candidate into the URL so back/forward
  // navigation and reloads-at-step-0 behave predictably. Heavier data
  // (questions, candidates, the review result) never round-trips through the
  // URL - only this component's state.
  useEffect(() => {
    const params = new URLSearchParams();
    if (state.step > 0) params.set('step', String(state.step));
    if (state.presetId) params.set('preset', state.presetId);
    if (state.candidateIndex !== null) params.set('candidate', String(state.candidateIndex));
    const qs = params.toString();
    router.replace(qs ? `/new/guided?${qs}` : '/new/guided');
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [state.step, state.presetId, state.candidateIndex]);

  // Picking a preset resets every downstream step's state synchronously (in
  // this event handler, not in the effect below) so the loading state that
  // triggers the fetch effect is set on the SAME render as the new preset -
  // mirrors the `pickPreset` pattern in the #31 exercise route.
  function pickPreset(preset: PresetSummaryDto) {
    setPresetObj(preset);
    setQuestionsState({ kind: 'loading' });
    setValues({});
    setAnswerError(null);
    setCandidatesState({ kind: 'idle' });
    setConfirmedPlan(null);
    dispatch({ type: 'select-preset', presetId: preset.id });
  }

  useEffect(() => {
    if (!presetObj) return;
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch(
          `/api/recommend/questions?preset=${encodeURIComponent(presetObj.id)}`,
        );
        const payload: unknown = await res.json().catch(() => null);
        if (cancelled) return;
        if (res.status === 401) {
          setQuestionsState({
            kind: 'error',
            message: 'Your session has expired. Reload the page or sign in again.',
          });
          return;
        }
        if (res.ok && looksLikeQuestions(payload)) {
          setQuestionsState({ kind: 'ready', questions: payload });
          return;
        }
        const message =
          (payload as { error?: string })?.error ?? `Could not load questions (${res.status}).`;
        setQuestionsState({ kind: 'error', message });
      } catch {
        if (!cancelled) {
          setQuestionsState({
            kind: 'error',
            message: 'Could not reach the server. Is it running?',
          });
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [presetObj]);

  function setValue(id: string, value: AnswerValue) {
    setValues((prev) => ({ ...prev, [id]: value }));
  }

  async function submitAnswers() {
    if (!presetObj || questionsState.kind !== 'ready') return;
    const built = buildAnswers(questionsState.questions, values);
    if (!built.ok) {
      setAnswerError(built.message);
      return;
    }
    setAnswerError(null);
    setRanking(true);
    setCandidatesState({ kind: 'idle' });
    try {
      const res = await fetch('/api/recommend/candidates', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ preset_id: presetObj.id, answers: built.answers }),
      });
      const payload: unknown = await res.json().catch(() => null);
      if (res.status === 401) {
        setCandidatesState({
          kind: 'error',
          message: 'Your session has expired. Reload the page or sign in again.',
        });
        return;
      }
      if (res.ok && looksLikeCandidatesResponse(payload)) {
        setCandidatesState({ kind: 'ready', candidates: payload.candidates });
        dispatch({
          type: 'submit-answers',
          answers: built.answers,
          candidates: payload.candidates,
        });
        return;
      }
      const message =
        (payload as { error?: string })?.error ?? `Could not rank candidates (${res.status}).`;
      setCandidatesState({ kind: 'error', message });
    } catch {
      setCandidatesState({ kind: 'error', message: 'Could not reach the server. Is it running?' });
    } finally {
      setRanking(false);
    }
  }

  function selectCandidate(index: number) {
    setConfirmedPlan(null);
    dispatch({ type: 'select-candidate', index });
  }

  // Going back to an earlier step resets every field the reducer clears for
  // that step (see `resetFromStep` in guided-steps.ts) AND the local
  // network/form state this component keeps alongside it, so the two never
  // drift out of sync.
  function backTo(step: number) {
    if (step < 1) {
      setPresetObj(null);
      setQuestionsState({ kind: 'idle' });
      setValues({});
      setAnswerError(null);
    }
    if (step < 2) {
      setCandidatesState({ kind: 'idle' });
    }
    if (step < 4) {
      setConfirmedPlan(null);
    }
    dispatch({ type: 'go-to-step', step });
  }

  const selectedCandidate =
    state.candidateIndex !== null ? (state.candidates?.[state.candidateIndex] ?? null) : null;

  return (
    <div className="space-y-6">
      <Stepper steps={STEP_LABELS} current={state.step} />

      {state.step === 0 ? <Gallery onSelect={pickPreset} selectedId={presetObj?.id} /> : null}

      {state.step === 1 && presetObj ? (
        <QuestionsStep
          presetName={presetObj.name}
          questionsState={questionsState}
          values={values}
          onChange={setValue}
          error={answerError}
          submitting={ranking}
          onBack={() => backTo(0)}
          onSubmit={() => void submitAnswers()}
        />
      ) : null}

      {state.step === 2 ? (
        <CandidatesStep
          candidatesState={candidatesState}
          selectedIndex={state.candidateIndex}
          onSelect={selectCandidate}
          onBack={() => backTo(1)}
        />
      ) : null}

      {state.step === 3 && selectedCandidate ? (
        <ReviewStep
          candidate={selectedCandidate}
          onBack={() => backTo(2)}
          onConfirm={(hcl, plan) => {
            setConfirmedPlan(plan);
            dispatch({ type: 'confirm-review', hcl });
          }}
        />
      ) : null}

      {state.step === LAST_STEP && state.confirmedHcl ? (
        <ConfirmStep hcl={state.confirmedHcl} plan={confirmedPlan} onBack={() => backTo(3)} />
      ) : null}
    </div>
  );
}

function QuestionsStep({
  presetName,
  questionsState,
  values,
  onChange,
  error,
  submitting,
  onBack,
  onSubmit,
}: {
  presetName: string;
  questionsState: QuestionsState;
  values: AnswerValues;
  onChange: (id: string, value: AnswerValue) => void;
  error: string | null;
  submitting: boolean;
  onBack: () => void;
  onSubmit: () => void;
}) {
  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between gap-2">
        <h2 className="text-lg font-medium">{presetName}</h2>
        <Button type="button" variant="outline" size="sm" onClick={onBack}>
          Change preset
        </Button>
      </div>

      {questionsState.kind === 'loading' ? <LoadingState label="Loading questions…" /> : null}
      {questionsState.kind === 'error' ? <ErrorAlert message={questionsState.message} /> : null}

      {questionsState.kind === 'ready' ? (
        <Card>
          <CardContent className="grid gap-4 p-4 sm:grid-cols-2">
            {questionsState.questions.map((q) => (
              <QuestionField
                key={q.id}
                question={q}
                value={values[q.id]}
                onChange={(v) => onChange(q.id, v)}
              />
            ))}
            {error ? (
              <div className="sm:col-span-2">
                <ErrorAlert message={error} />
              </div>
            ) : null}
            <div className="sm:col-span-2">
              <Button type="button" onClick={onSubmit} disabled={submitting}>
                {submitting ? 'Ranking…' : 'Get candidates'}
              </Button>
            </div>
          </CardContent>
        </Card>
      ) : null}
    </div>
  );
}

function CandidatesStep({
  candidatesState,
  selectedIndex,
  onSelect,
  onBack,
}: {
  candidatesState: CandidatesState;
  selectedIndex: number | null;
  onSelect: (index: number) => void;
  onBack: () => void;
}) {
  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between gap-2">
        <h2 className="text-lg font-medium">Candidates</h2>
        <Button type="button" variant="outline" size="sm" onClick={onBack}>
          Back to questions
        </Button>
      </div>

      {candidatesState.kind === 'error' ? <ErrorAlert message={candidatesState.message} /> : null}
      {candidatesState.kind === 'ready' ? (
        <CandidateGrid
          candidates={candidatesState.candidates}
          selectedIndex={selectedIndex}
          onSelect={onSelect}
        />
      ) : null}
    </div>
  );
}

function ReviewStep({
  candidate,
  onBack,
  onConfirm,
}: {
  candidate: CandidateDto;
  onBack: () => void;
  onConfirm: (hcl: string, plan: Plan) => void;
}) {
  const estimate = candidate.estimate;
  // Whether the editor's text currently differs from the candidate's original
  // seed HCL - lifted from `CandidateReview` via `onDirtyChange`, called from
  // its own edit handler (not an effect), so this never lags a render behind.
  const [dirty, setDirty] = useState(false);

  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between gap-2">
        <h2 className="text-lg font-medium">Review &amp; customize</h2>
        <Button type="button" variant="outline" size="sm" onClick={onBack}>
          Back to candidates
        </Button>
      </div>

      {/* The candidate's own estimate, shown purely for context. It is NEVER
          recomputed here - `POST /recommend/customize` deliberately returns no
          estimate for edited HCL, so this figure is only ever accurate for the
          candidate exactly as proposed. Labeled as stale once the editor below
          has actually been touched, rather than unconditionally. */}
      {estimate ? (
        <Card>
          <CardContent className="space-y-2 p-4">
            <div className="flex items-center justify-between gap-2">
              <h3 className="text-sm font-medium">Original candidate estimate</h3>
              {dirty ? <Badge variant="outline">Stale after edits</Badge> : null}
            </div>
            <p className="text-xs text-muted-foreground">
              Computed for {archetypeLabel(candidate.archetype)} exactly as proposed. Editing the
              HCL below does not update it - reviewing changes re-validates and re-plans your
              edits, but does not produce a new estimate.
            </p>
            <dl className="grid gap-3 sm:grid-cols-2">
              <div className="space-y-0.5">
                <dt className="text-xs text-muted-foreground">Time to first token (TTFT)</dt>
                <dd className="font-mono text-sm">{formatRangeMs(estimate.expected_ttft_ms)}</dd>
              </div>
              <div className="space-y-0.5">
                <dt className="text-xs text-muted-foreground">Time per output token (TPOT)</dt>
                <dd className="font-mono text-sm">{formatRangeMs(estimate.expected_tpot_ms)}</dd>
              </div>
            </dl>
          </CardContent>
        </Card>
      ) : null}

      <CandidateReview candidate={candidate} onConfirm={onConfirm} onDirtyChange={setDirty} />
    </div>
  );
}

function ConfirmStep({
  hcl,
  plan,
  onBack,
}: {
  hcl: string;
  plan: Plan | null;
  onBack: () => void;
}) {
  return (
    <div className="space-y-5">
      <div className="flex items-center justify-between gap-2">
        <h2 className="text-lg font-medium">Confirm &amp; create</h2>
        <Button type="button" variant="outline" size="sm" onClick={onBack}>
          Back to review
        </Button>
      </div>

      <Alert variant="success" className="bg-success/10">
        <AlertDescription className="text-success">
          Plan reviewed and confirmed. Save it below to create a new configuration in My Configs -
          nothing is applied.
        </AlertDescription>
      </Alert>

      {plan ? (
        <div className="space-y-2">
          <h3 className="text-sm font-medium">Confirmed plan</h3>
          <PlanPanel plan={plan} />
        </div>
      ) : null}

      <SaveConfigForm hcl={hcl} />
    </div>
  );
}
