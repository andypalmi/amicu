'use client';

import { useEffect, useState } from 'react';
import Link from 'next/link';
import { useSearchParams } from 'next/navigation';
import {
  Badge,
  Button,
  Card,
  CardContent,
  Input,
  Label,
  Select,
} from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { LoadingState } from '@/components/common/loading-state';
import { Gallery } from './gallery';
import { HclResult } from './hcl-result';
import { Stepper } from './stepper';
import {
  modalityLabel,
  presetRequiresEuResidency,
  type EmitResponseDto,
  type PresetDetailDto,
  type PresetSummaryDto,
  type WizardAnswersDto,
} from '@/lib/wizard/format';
import { errorMessage, looksLikeConfigDetail } from '@/lib/configs/format';

// Exported so the guided wizard's <QuestionForm>
// can reuse the same currency list for its money-question control instead of
// re-declaring it.
export const CURRENCIES = ['USD', 'EUR', 'GBP'] as const;

const STEPS = ['Pick a preset', 'Requirements', 'Review & emit'] as const;

// The three-step requirements form. Numbers are held as strings while editing and
// parsed on submit; only the fields WizardAnswersDto actually carries are sent.
type FormState = {
  contextLength: string;
  concurrency: string;
  maxLatency: string;
  euResidency: boolean;
  budgetAmount: string;
  budgetCurrency: string;
};

type DetailState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; detail: PresetDetailDto };

type EmitState =
  | { kind: 'idle' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; result: EmitResponseDto };

function looksLikeDetail(body: unknown): body is PresetDetailDto {
  return (
    !!body &&
    typeof body === 'object' &&
    typeof (body as { summary?: unknown }).summary === 'object'
  );
}

function looksLikeEmit(body: unknown): body is EmitResponseDto {
  return (
    !!body &&
    typeof body === 'object' &&
    typeof (body as { hcl?: unknown }).hcl === 'string' &&
    typeof (body as { estimate?: unknown }).estimate === 'object'
  );
}

// Loading a config into the wizard via `?configId=`: fetched once from
// GET /api/configs/{id} and rendered straight into <HclResult> (skipping the
// preset/requirements steps - there is no preset behind a loaded config, so
// nothing is re-emitted or re-estimated).
// Result of the last load attempt, keyed by the config id it belongs to - so a
// stale result from a PREVIOUS `?configId=` is never shown while a new one is
// in flight (see `isLoading` below, which compares `loaded.id` to the current
// `loadConfigId` instead of tracking a separate "loading" boolean in state).
type LoadedConfigResult =
  | { kind: 'error'; id: string; message: string }
  | { kind: 'ready'; id: string; name: string; hcl: string };

// The classic preset wizard: a client stepper over the preset BFF routes. (1)
// pick a preset from the gallery; (2) fill a short requirements form seeded
// from the preset's detail; (3) emit the HCL and review it. No tier gating
// here - the endpoints are Community-minimum and the API enforces tier by
// content.
//
// A `?configId=<id>` query param instead loads a saved configuration (from "My
// Configs" - see components/configs/detail.tsx's "Load into classic wizard"
// link) and renders it straight into the same review/editor/Save panel,
// bypassing the preset flow entirely.
export function Wizard() {
  const searchParams = useSearchParams();
  const loadConfigId = searchParams.get('configId');
  const [loaded, setLoaded] = useState<LoadedConfigResult | null>(null);

  useEffect(() => {
    if (!loadConfigId) return;
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch(`/api/configs/${encodeURIComponent(loadConfigId)}`);
        const payload: unknown = await res.json().catch(() => null);
        if (cancelled) return;
        if (res.ok && looksLikeConfigDetail(payload)) {
          setLoaded({ kind: 'ready', id: payload.id, name: payload.name, hcl: payload.content });
          return;
        }
        setLoaded({ kind: 'error', id: loadConfigId, message: errorMessage(res.status, payload) });
      } catch {
        if (!cancelled) {
          setLoaded({
            kind: 'error',
            id: loadConfigId,
            message: 'The configuration service is unreachable.',
          });
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [loadConfigId]);

  if (loadConfigId) {
    const current = loaded && loaded.id === loadConfigId ? loaded : null;
    return <LoadedConfig loading={current === null} result={current} />;
  }

  return <PresetWizard />;
}

function LoadedConfig({
  loading,
  result,
}: {
  loading: boolean;
  result: LoadedConfigResult | null;
}) {
  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="space-y-1">
          <h2 className="text-lg font-medium">
            {result?.kind === 'ready' ? `Editing "${result.name}"` : 'Loading configuration…'}
          </h2>
          <p className="text-sm text-muted-foreground">
            Loaded from My Configs. Edit it below, then Save to add a new version.
          </p>
        </div>
        <Button asChild variant="outline" size="sm">
          <Link href="/wizard">Start the wizard from scratch</Link>
        </Button>
      </div>

      {loading ? <LoadingState label="Loading configuration…" /> : null}
      {result?.kind === 'error' ? <ErrorAlert message={result.message} /> : null}
      {result?.kind === 'ready' ? (
        <HclResult
          result={{ hcl: result.hcl }}
          sourceConfigId={result.id}
          sourceConfigName={result.name}
        />
      ) : null}
    </div>
  );
}

// The original three-step preset flow: pick a preset, fill requirements, emit
// and review. Extracted so `Wizard` can branch to <LoadedConfig> without this
// component's hooks running unconditionally.
function PresetWizard() {
  const [step, setStep] = useState(0);
  const [selected, setSelected] = useState<PresetSummaryDto | null>(null);
  const [detailState, setDetailState] = useState<DetailState>({ kind: 'idle' });
  const [form, setForm] = useState<FormState>({
    contextLength: '',
    concurrency: '',
    maxLatency: '',
    euResidency: false,
    budgetAmount: '',
    budgetCurrency: 'USD',
  });
  const [formError, setFormError] = useState<string | null>(null);
  const [emitting, setEmitting] = useState(false);
  const [emit, setEmit] = useState<EmitState>({ kind: 'idle' });

  // Fetch the preset detail and seed the form whenever a preset is picked. The
  // loading state is set in the pickPreset handler (not here) so the effect body
  // never calls setState synchronously.
  useEffect(() => {
    if (!selected) return;
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch(`/api/presets/${encodeURIComponent(selected.id)}`);
        const payload: unknown = await res.json().catch(() => null);
        if (cancelled) return;
        if (res.status === 401) {
          setDetailState({
            kind: 'error',
            message: 'Your session has expired. Reload the page or sign in again.',
          });
          return;
        }
        if (res.status === 404) {
          setDetailState({ kind: 'error', message: 'That preset no longer exists.' });
          return;
        }
        if (res.ok && looksLikeDetail(payload)) {
          const defaults = payload.requirement_defaults;
          setForm((prev) => ({
            ...prev,
            contextLength:
              defaults.min_context_length != null ? String(defaults.min_context_length) : '',
            concurrency: String(payload.default_estimate.assumed_concurrency),
            maxLatency: defaults.max_latency_ms != null ? String(defaults.max_latency_ms) : '',
            euResidency: presetRequiresEuResidency(payload.summary),
          }));
          setDetailState({ kind: 'ready', detail: payload });
          return;
        }
        const message =
          (payload as { error?: string })?.error ?? `Could not load the preset (${res.status}).`;
        setDetailState({ kind: 'error', message });
      } catch {
        if (!cancelled) {
          setDetailState({ kind: 'error', message: 'Could not reach the server. Is it running?' });
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [selected]);

  function pickPreset(preset: PresetSummaryDto) {
    setSelected(preset);
    setDetailState({ kind: 'loading' });
    setEmit({ kind: 'idle' });
    setFormError(null);
    setStep(1);
  }

  async function submit() {
    if (!selected) return;
    const contextLength = Number(form.contextLength);
    const concurrency = Number(form.concurrency);
    if (!Number.isFinite(contextLength) || contextLength <= 0) {
      setFormError('Context length must be a positive number of tokens.');
      return;
    }
    if (!Number.isFinite(concurrency) || concurrency <= 0) {
      setFormError('Concurrency must be a positive number of requests.');
      return;
    }

    // Hard-EU presets lock the residency requirement on: OR it in explicitly here
    // rather than trusting a disabled control's submitted value.
    const answers: WizardAnswersDto = {
      context_length: Math.trunc(contextLength),
      concurrency: Math.trunc(concurrency),
      eu_residency_required: presetRequiresEuResidency(selected) || form.euResidency,
    };
    if (form.maxLatency.trim()) {
      const ms = Number(form.maxLatency);
      if (!Number.isFinite(ms) || ms <= 0) {
        setFormError('Max latency must be a positive number of milliseconds, or left blank.');
        return;
      }
      answers.max_latency_ms = Math.trunc(ms);
    }
    if (form.budgetAmount.trim()) {
      const amount = Number(form.budgetAmount);
      if (!Number.isFinite(amount) || amount < 0) {
        setFormError('Budget must be a non-negative amount, or left blank.');
        return;
      }
      answers.budget = { amount, currency: form.budgetCurrency };
    }

    setFormError(null);
    setEmitting(true);
    setEmit({ kind: 'idle' });
    setStep(2);
    try {
      const res = await fetch(`/api/presets/${encodeURIComponent(selected.id)}/emit`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(answers),
      });
      const payload: unknown = await res.json().catch(() => null);
      if (res.status === 401) {
        setEmit({
          kind: 'error',
          message: 'Your session has expired. Reload the page or sign in again.',
        });
        return;
      }
      if (res.ok && looksLikeEmit(payload)) {
        setEmit({ kind: 'ready', result: payload });
        return;
      }
      const message =
        (payload as { error?: string })?.error ?? `Could not emit the configuration (${res.status}).`;
      setEmit({ kind: 'error', message });
    } catch {
      setEmit({ kind: 'error', message: 'Could not reach the server. Is it running?' });
    } finally {
      setEmitting(false);
    }
  }

  return (
    <div className="space-y-6">
      <Stepper steps={STEPS} current={step} />

      {step === 0 ? (
        <Gallery onSelect={pickPreset} selectedId={selected?.id} />
      ) : null}

      {step === 1 ? (
        <RequirementsStep
          selected={selected}
          detailState={detailState}
          form={form}
          setForm={setForm}
          formError={formError}
          onBack={() => setStep(0)}
          onSubmit={submit}
        />
      ) : null}

      {step === 2 ? (
        <ReviewStep
          selectedName={selected?.name}
          emitting={emitting}
          emit={emit}
          onBack={() => setStep(1)}
          onRestart={() => {
            setSelected(null);
            setDetailState({ kind: 'idle' });
            setEmit({ kind: 'idle' });
            setStep(0);
          }}
        />
      ) : null}
    </div>
  );
}

function RequirementsStep({
  selected,
  detailState,
  form,
  setForm,
  formError,
  onBack,
  onSubmit,
}: {
  selected: PresetSummaryDto | null;
  detailState: DetailState;
  form: FormState;
  setForm: React.Dispatch<React.SetStateAction<FormState>>;
  formError: string | null;
  onBack: () => void;
  onSubmit: () => void;
}) {
  if (!selected) return null;

  const detail = detailState.kind === 'ready' ? detailState.detail : null;
  const modalities = detail?.requirement_defaults.modalities ?? [];
  const dataClass = detail?.requirement_defaults.required_data_class ?? null;
  // A hard-EU preset commits to EU residency: lock the control on so a user cannot
  // untick it and emit non-EU HCL from an EU preset. submit() enforces the same.
  const euLocked = presetRequiresEuResidency(selected);

  return (
    <div className="space-y-5">
      <div className="space-y-1">
        <h2 className="text-lg font-medium">{selected.name}</h2>
        <p className="text-sm text-muted-foreground">{selected.tagline}</p>
      </div>

      {detailState.kind === 'loading' ? <LoadingState label="Loading preset defaults…" /> : null}
      {detailState.kind === 'error' ? <ErrorAlert message={detailState.message} /> : null}

      {detail ? (
        <>
          {/* Seeded, display-only context. Modality and data class come from the
              preset's use case; they are NOT part of the emit request body. */}
          {(modalities.length > 0 || dataClass) && (
            <Card>
              <CardContent className="space-y-2 p-4 text-sm">
                <p className="text-xs font-medium text-muted-foreground">
                  From this preset (context, not editable)
                </p>
                <div className="flex flex-wrap items-center gap-1.5">
                  {modalities.map((m) => (
                    <Badge key={m} variant="secondary">
                      {modalityLabel(m)}
                    </Badge>
                  ))}
                  {dataClass ? (
                    <Badge variant="outline" title="Required data class">
                      Data class: {dataClass}
                    </Badge>
                  ) : null}
                </div>
              </CardContent>
            </Card>
          )}

          <form
            className="grid gap-5 sm:grid-cols-2"
            onSubmit={(e) => {
              e.preventDefault();
              onSubmit();
            }}
          >
            <div className="space-y-1.5">
              <Label htmlFor="contextLength">Context length (tokens)</Label>
              <Input
                id="contextLength"
                type="number"
                min={1}
                inputMode="numeric"
                value={form.contextLength}
                onChange={(e) => setForm((p) => ({ ...p, contextLength: e.target.value }))}
                required
              />
              <p className="text-xs text-muted-foreground">
                The context window the runtime is configured for.
              </p>
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="concurrency">Concurrency (requests)</Label>
              <Input
                id="concurrency"
                type="number"
                min={1}
                inputMode="numeric"
                value={form.concurrency}
                onChange={(e) => setForm((p) => ({ ...p, concurrency: e.target.value }))}
                required
              />
              <p className="text-xs text-muted-foreground">
                Expected concurrent requests. Drives GPU count.
              </p>
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="maxLatency">Max latency (ms, optional)</Label>
              <Input
                id="maxLatency"
                type="number"
                min={1}
                inputMode="numeric"
                placeholder="No ceiling"
                value={form.maxLatency}
                onChange={(e) => setForm((p) => ({ ...p, maxLatency: e.target.value }))}
              />
              <p className="text-xs text-muted-foreground">
                An optional latency ceiling carried into the use case.
              </p>
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="euResidency">Data residency</Label>
              <Select
                id="euResidency"
                value={euLocked || form.euResidency ? 'eu' : 'any'}
                disabled={euLocked}
                onChange={(e) =>
                  setForm((p) => ({ ...p, euResidency: e.target.value === 'eu' }))
                }
              >
                <option value="any">Any region</option>
                <option value="eu">EU jurisdictions only</option>
              </Select>
              <p className="text-xs text-muted-foreground">
                {euLocked
                  ? 'This preset requires EU residency, so this is locked on.'
                  : 'Whether the workload must stay within EU jurisdictions.'}
              </p>
            </div>

            <div className="space-y-1.5">
              <Label htmlFor="budgetAmount">Budget (optional)</Label>
              <div className="flex gap-2">
                <Input
                  id="budgetAmount"
                  type="number"
                  min={0}
                  step="any"
                  inputMode="decimal"
                  placeholder="No budget"
                  value={form.budgetAmount}
                  onChange={(e) => setForm((p) => ({ ...p, budgetAmount: e.target.value }))}
                  className="flex-1"
                />
                <Select
                  aria-label="Budget currency"
                  value={form.budgetCurrency}
                  onChange={(e) => setForm((p) => ({ ...p, budgetCurrency: e.target.value }))}
                  className="w-24"
                >
                  {CURRENCIES.map((c) => (
                    <option key={c} value={c}>
                      {c}
                    </option>
                  ))}
                </Select>
              </div>
              <p className="text-xs text-muted-foreground">A rough monthly budget ceiling.</p>
            </div>

            {formError ? (
              <div className="sm:col-span-2">
                <ErrorAlert message={formError} />
              </div>
            ) : null}

            <div className="flex flex-wrap items-center gap-2 sm:col-span-2">
              <Button type="button" variant="outline" onClick={onBack}>
                Back
              </Button>
              <Button type="submit">Emit configuration</Button>
            </div>
          </form>
        </>
      ) : null}
    </div>
  );
}

function ReviewStep({
  selectedName,
  emitting,
  emit,
  onBack,
  onRestart,
}: {
  selectedName?: string;
  emitting: boolean;
  emit: EmitState;
  onBack: () => void;
  onRestart: () => void;
}) {
  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="space-y-1">
          <h2 className="text-lg font-medium">Review &amp; emit</h2>
          {selectedName ? (
            <p className="text-sm text-muted-foreground">
              Generated from <span className="font-medium">{selectedName}</span>.
            </p>
          ) : null}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          <Button type="button" variant="outline" size="sm" onClick={onBack}>
            Back to requirements
          </Button>
          <Button type="button" variant="outline" size="sm" onClick={onRestart}>
            Start over
          </Button>
        </div>
      </div>

      {emitting ? <LoadingState label="Emitting configuration…" /> : null}
      {emit.kind === 'error' ? <ErrorAlert message={emit.message} /> : null}
      {emit.kind === 'ready' ? <HclResult result={emit.result} /> : null}
    </div>
  );
}
