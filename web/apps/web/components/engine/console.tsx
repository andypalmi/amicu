'use client';

import { useState } from 'react';
import { Button, Textarea } from '@amicu/ui';
import { EXAMPLE_CONFIG } from '@/lib/engine/example';
import { isTierDenied, type DiagnosticReport, type Plan } from '@/lib/engine/format';
import { ErrorAlert } from '@/components/common/error-alert';
import { DiagnosticsPanel } from './diagnostics-panel';
import { PlanPanel } from './plan-panel';

type Result =
  | { kind: 'idle' }
  | { kind: 'diagnostics'; report: DiagnosticReport }
  | { kind: 'plan'; plan: Plan }
  | { kind: 'error'; message: string; upgrade?: boolean };

type Pending = 'validate' | 'plan' | null;

function looksLikeReport(body: unknown): body is DiagnosticReport {
  return (
    !!body &&
    typeof body === 'object' &&
    Array.isArray((body as { diagnostics?: unknown }).diagnostics)
  );
}

function looksLikePlan(body: unknown): body is Plan {
  return (
    !!body &&
    typeof body === 'object' &&
    Array.isArray((body as { actions?: unknown }).actions)
  );
}

// The authenticated engine console: an HCL editor wired to the /validate and
// /plan BFF routes, with an optional prior-config pane so /plan renders a diff.
// All state is client-side; the BFF holds the session and talks to the API.
export function EngineConsole() {
  const [config, setConfig] = useState(EXAMPLE_CONFIG);
  const [prior, setPrior] = useState('');
  const [diffMode, setDiffMode] = useState(false);
  const [pending, setPending] = useState<Pending>(null);
  const [result, setResult] = useState<Result>({ kind: 'idle' });

  async function run(endpoint: 'validate' | 'plan') {
    setPending(endpoint);
    setResult({ kind: 'idle' });
    const body =
      endpoint === 'plan' && diffMode
        ? { config, prior: prior.trim() ? prior : null }
        : { config };
    try {
      const res = await fetch(`/api/engine/${endpoint}`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      const payload: unknown = await res.json().catch(() => null);

      if (res.status === 401) {
        setResult({
          kind: 'error',
          message: 'Your session has expired. Reload the page or sign in again.',
        });
        return;
      }
      if (isTierDenied(res.status)) {
        setResult({
          kind: 'error',
          message: 'This feature requires a higher plan. The engine is gated by the API.',
          upgrade: true,
        });
        return;
      }
      if (res.ok) {
        if (endpoint === 'validate' && looksLikeReport(payload)) {
          setResult({ kind: 'diagnostics', report: payload });
        } else if (endpoint === 'plan' && looksLikePlan(payload)) {
          setResult({ kind: 'plan', plan: payload });
        } else {
          setResult({ kind: 'error', message: 'Unexpected response from the server.' });
        }
        return;
      }
      // A valid-but-unplannable config comes back 422 with a DiagnosticReport.
      if (res.status === 422 && looksLikeReport(payload)) {
        setResult({ kind: 'diagnostics', report: payload });
        return;
      }
      // 400 parse error, 502 upstream, etc. - surface the API's own message.
      const message =
        (payload as { error?: string })?.error ?? `Request failed (${res.status}).`;
      setResult({ kind: 'error', message });
    } catch {
      setResult({ kind: 'error', message: 'Could not reach the server. Is it running?' });
    } finally {
      setPending(null);
    }
  }

  // The editor keeps its taller sizing, resize handle, and mono type; the kit
  // <Textarea> supplies the uniform border/focus/disabled treatment.
  const editorClass = 'min-h-[18rem] resize-y font-mono text-xs leading-relaxed';

  return (
    <div className="grid gap-6 lg:grid-cols-2">
      {/* Editor column */}
      <section className="space-y-3">
        <div className="flex items-center justify-between gap-2">
          <label htmlFor="config" className="text-sm font-medium">
            Configuration
          </label>
          <Button
            type="button"
            variant="link"
            size="sm"
            onClick={() => setConfig(EXAMPLE_CONFIG)}
          >
            Reset to example
          </Button>
        </div>
        <Textarea
          id="config"
          value={config}
          onChange={(e) => setConfig(e.target.value)}
          spellCheck={false}
          className={editorClass}
          aria-label="Amicu HCL configuration"
        />

        <label className="flex items-center gap-2 text-sm">
          <input
            type="checkbox"
            checked={diffMode}
            onChange={(e) => setDiffMode(e.target.checked)}
            className="h-4 w-4 rounded border-border"
          />
          Diff against a prior configuration
        </label>

        {diffMode ? (
          <div className="space-y-2">
            <label htmlFor="prior" className="text-sm font-medium text-muted-foreground">
              Prior configuration
            </label>
            <Textarea
              id="prior"
              value={prior}
              onChange={(e) => setPrior(e.target.value)}
              spellCheck={false}
              placeholder="Leave empty to plan everything as a fresh create."
              className={editorClass}
              aria-label="Prior Amicu HCL configuration"
            />
          </div>
        ) : null}

        <div className="flex flex-wrap items-center gap-2">
          <Button
            onClick={() => run('validate')}
            disabled={pending !== null}
            variant="outline"
          >
            {pending === 'validate' ? 'Validating…' : 'Validate'}
          </Button>
          <Button onClick={() => run('plan')} disabled={pending !== null}>
            {pending === 'plan' ? 'Planning…' : diffMode ? 'Plan diff' : 'Plan'}
          </Button>
        </div>
      </section>

      {/* Results column */}
      <section className="space-y-3">
        <h2 className="text-sm font-medium">Result</h2>
        {result.kind === 'idle' ? (
          <p className="text-sm text-muted-foreground">
            {pending
              ? 'Running…'
              : 'Validate to check the configuration, or Plan to see the actions Amicu would take.'}
          </p>
        ) : null}

        {result.kind === 'error' ? (
          <ErrorAlert message={result.message} upgrade={result.upgrade} />
        ) : null}

        {result.kind === 'diagnostics' ? (
          <DiagnosticsPanel report={result.report} />
        ) : null}

        {result.kind === 'plan' ? <PlanPanel plan={result.plan} /> : null}
      </section>
    </div>
  );
}
