'use client';

import { useEffect, useState } from 'react';
import { Alert, AlertDescription, Badge, Button, Card, CardContent, Input, Label, Select } from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { EmptyState } from '@/components/common/empty-state';
import { LoadingState } from '@/components/common/loading-state';
import { Gallery } from './gallery';
import { CandidateReview } from './candidate-review';
import type { PresetSummaryDto } from '@/lib/wizard/format';
import {
  archetypeLabel,
  looksLikeCandidatesResponse,
  looksLikeQuestions,
  type CandidateDto,
  type QuestionDto,
} from '@/lib/recommend/candidates';
import type { Plan } from '@/lib/engine/format';

type QuestionsState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; questions: QuestionDto[] };

type CandidatesState =
  | { kind: 'idle' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; candidates: CandidateDto[] };

// Raw per-question field state, keyed by QuestionDto.id. Kept as
// strings/booleans while editing; cast to the WizardAnswersDto shape
// /recommend/candidates expects only at submit time (see `rankCandidates`).
type AnswerValues = Record<string, string | boolean>;

type ConfirmedState = { hcl: string; plan: Plan } | null;

// The #31/#35 "candidates" wizard exercise route: pick a preset, answer its
// engine-provided core questions (GET /api/recommend/questions), rank
// candidates (POST /api/recommend/candidates), and review/edit/confirm one via
// <CandidateReview> (POST /api/recommend/customize). Deliberately plain - no
// stepper polish, nothing persisted on confirm. #85 owns the production
// `/new`/`/new/guided` guided-wizard surface; this only exercises #31's and
// #35's endpoints end to end.
export function CandidatePicker() {
  const [preset, setPreset] = useState<PresetSummaryDto | null>(null);
  const [questionsState, setQuestionsState] = useState<QuestionsState>({ kind: 'idle' });
  const [values, setValues] = useState<AnswerValues>({});
  const [candidatesState, setCandidatesState] = useState<CandidatesState>({ kind: 'idle' });
  const [ranking, setRanking] = useState(false);
  const [selected, setSelected] = useState<CandidateDto | null>(null);
  const [confirmed, setConfirmed] = useState<ConfirmedState>(null);

  // Picking a preset resets every downstream step's state synchronously (in
  // this event handler, not in the effect below) so the loading state that
  // triggers the fetch effect is set on the SAME render as the new `preset`,
  // never as a second cascading render from inside the effect.
  function pickPreset(p: PresetSummaryDto) {
    setPreset(p);
    setQuestionsState({ kind: 'loading' });
    setValues({});
    setCandidatesState({ kind: 'idle' });
    setSelected(null);
    setConfirmed(null);
  }

  useEffect(() => {
    if (!preset) return;
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch(
          `/api/recommend/questions?preset=${encodeURIComponent(preset.id)}`,
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
  }, [preset]);

  function setValue(id: string, value: string | boolean) {
    setValues((prev) => ({ ...prev, [id]: value }));
  }

  async function rankCandidates() {
    if (!preset || questionsState.kind !== 'ready') return;

    const answers: Record<string, unknown> = {};
    for (const q of questionsState.questions) {
      const raw = values[q.id];
      if (raw === undefined || raw === '') continue;
      switch (q.kind.type) {
        case 'number':
          answers[q.id] = Number(raw);
          break;
        case 'boolean':
          answers[q.id] = Boolean(raw);
          break;
        case 'money':
          answers[q.id] = { amount: Number(raw), currency: 'USD' };
          break;
        default:
          answers[q.id] = raw;
      }
    }
    if (typeof answers.context_length !== 'number' || typeof answers.concurrency !== 'number') {
      setCandidatesState({
        kind: 'error',
        message: 'Context length and concurrency are required.',
      });
      return;
    }

    setRanking(true);
    setCandidatesState({ kind: 'idle' });
    setSelected(null);
    setConfirmed(null);
    try {
      const res = await fetch('/api/recommend/candidates', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ preset_id: preset.id, answers }),
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

  if (!preset) {
    return <Gallery onSelect={pickPreset} />;
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between gap-2">
        <h2 className="text-lg font-medium">{preset.name}</h2>
        <Button type="button" variant="outline" size="sm" onClick={() => setPreset(null)}>
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
                onChange={(v) => setValue(q.id, v)}
              />
            ))}
            <div className="sm:col-span-2">
              <Button type="button" onClick={() => void rankCandidates()} disabled={ranking}>
                {ranking ? 'Ranking…' : 'Get candidates'}
              </Button>
            </div>
          </CardContent>
        </Card>
      ) : null}

      {candidatesState.kind === 'error' ? <ErrorAlert message={candidatesState.message} /> : null}

      {candidatesState.kind === 'ready' ? (
        candidatesState.candidates.length === 0 ? (
          <EmptyState
            title="No candidates"
            description="No candidate satisfied every hard constraint for these answers."
          />
        ) : (
          <div className="grid gap-3 sm:grid-cols-2">
            {candidatesState.candidates.map((c, i) => {
              const active = selected === c;
              return (
                <Card
                  key={i}
                  className={active ? 'border-foreground/40 ring-1 ring-foreground/20' : undefined}
                >
                  <CardContent className="space-y-2 p-4">
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-sm font-medium">{archetypeLabel(c.archetype)}</span>
                      <Badge variant="outline">score {c.score.total.toFixed(2)}</Badge>
                    </div>
                    {c.rationale[0] ? (
                      <p className="text-xs text-muted-foreground">{c.rationale[0]}</p>
                    ) : null}
                    <Button
                      type="button"
                      size="sm"
                      variant={active ? 'default' : 'outline'}
                      className="w-full"
                      onClick={() => {
                        setSelected(c);
                        setConfirmed(null);
                      }}
                    >
                      {active ? 'Selected' : 'Review this candidate'}
                    </Button>
                  </CardContent>
                </Card>
              );
            })}
          </div>
        )
      ) : null}

      {selected ? (
        <CandidateReview candidate={selected} onConfirm={(hcl, plan) => setConfirmed({ hcl, plan })} />
      ) : null}

      {confirmed ? (
        <Alert variant="success" className="bg-success/10">
          <AlertDescription className="text-success">
            <span className="font-medium">Confirmed.</span> The reviewed configuration and its plan
            (
            {confirmed.plan.actions.length} action{confirmed.plan.actions.length === 1 ? '' : 's'}
            ) were accepted. Nothing was applied - #35 stops at confirmation.
          </AlertDescription>
        </Alert>
      ) : null}
    </div>
  );
}

function QuestionField({
  question,
  value,
  onChange,
}: {
  question: QuestionDto;
  value: string | boolean | undefined;
  onChange: (value: string | boolean) => void;
}) {
  const { kind } = question;

  return (
    <div className="space-y-1.5">
      <Label htmlFor={question.id}>{question.prompt}</Label>
      {kind.type === 'boolean' ? (
        <div className="flex items-center gap-2">
          <input
            id={question.id}
            type="checkbox"
            checked={value === true}
            onChange={(e) => onChange(e.target.checked)}
            className="h-4 w-4"
          />
        </div>
      ) : kind.type === 'choice' ? (
        <Select
          id={question.id}
          value={typeof value === 'string' ? value : ''}
          onChange={(e) => onChange(e.target.value)}
        >
          <option value="">Balanced (default)</option>
          {kind.options.map((o) => (
            <option key={o} value={o}>
              {o}
            </option>
          ))}
        </Select>
      ) : (
        <Input
          id={question.id}
          type={kind.type === 'number' || kind.type === 'money' ? 'number' : 'text'}
          inputMode={kind.type === 'number' || kind.type === 'money' ? 'decimal' : undefined}
          min={kind.type === 'number' ? (kind.min ?? undefined) : undefined}
          max={kind.type === 'number' ? (kind.max ?? undefined) : undefined}
          required={question.required}
          value={typeof value === 'string' ? value : ''}
          onChange={(e) => onChange(e.target.value)}
        />
      )}
      {question.help ? <p className="text-xs text-muted-foreground">{question.help}</p> : null}
      {kind.type === 'number' && kind.unit ? (
        <p className="text-xs text-muted-foreground">Unit: {kind.unit}</p>
      ) : null}
      {kind.type === 'money' ? (
        <p className="text-xs text-muted-foreground">Currency: USD.</p>
      ) : null}
    </div>
  );
}
