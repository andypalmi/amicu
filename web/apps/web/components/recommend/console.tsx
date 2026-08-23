'use client';

import { useState } from 'react';
import Link from 'next/link';
import { Button, Card, CardContent, Textarea } from '@amicu/ui';
import { EXAMPLE_SNAPSHOT } from '@/lib/recommend/example';
import { isTierDenied, type TuningReport } from '@/lib/recommend/format';
import { useTier } from '@/components/session-provider';
import { ErrorAlert } from '@/components/common/error-alert';
import { EmptyState } from '@/components/common/empty-state';
import { RecommendationCard } from './recommendation-card';

type Result =
  | { kind: 'idle' }
  | { kind: 'report'; report: TuningReport }
  | { kind: 'error'; message: string; upgrade?: boolean };

function looksLikeReport(body: unknown): body is TuningReport {
  return (
    !!body &&
    typeof body === 'object' &&
    Array.isArray((body as { recommendations?: unknown }).recommendations)
  );
}

// The advisory recommendation console: paste a measured metrics + estimate
// snapshot, POST it to the /api/recommend BFF, and render the returned tuning
// report with full transparency. All ranking/rationale comes from the API - this
// component only presents it and never authors advice.
export function RecommendConsole() {
  const tier = useTier();
  const [snapshot, setSnapshot] = useState(EXAMPLE_SNAPSHOT);
  const [pending, setPending] = useState(false);
  const [result, setResult] = useState<Result>({ kind: 'idle' });

  async function run() {
    // Parse locally so obviously-malformed JSON is caught before a round trip.
    let body: unknown;
    try {
      body = JSON.parse(snapshot);
    } catch {
      setResult({ kind: 'error', message: 'The snapshot is not valid JSON.' });
      return;
    }

    setPending(true);
    setResult({ kind: 'idle' });
    try {
      const res = await fetch('/api/recommend', {
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
          message: 'Recommendations require a higher plan. The engine is gated by the API.',
          upgrade: true,
        });
        return;
      }
      if (res.ok && looksLikeReport(payload)) {
        setResult({ kind: 'report', report: payload });
        return;
      }
      if (res.status === 422) {
        // A well-formed-JSON body whose nested fields have the wrong type reaches
        // the API's extractor, which answers 422 with a plain-text (non-JSON)
        // body - so don't rely on payload.error here.
        setResult({
          kind: 'error',
          message:
            'The request body was malformed (a field had the wrong type). Check the metrics and estimate JSON.',
        });
        return;
      }
      // 400 malformed body, 502 upstream, etc. - surface the API's own message.
      const message =
        (payload as { error?: string })?.error ?? `Request failed (${res.status}).`;
      setResult({ kind: 'error', message });
    } catch {
      setResult({ kind: 'error', message: 'Could not reach the server. Is it running?' });
    } finally {
      setPending(false);
    }
  }

  // The editor keeps its taller sizing, resize handle, and mono type; the kit
  // <Textarea> supplies the uniform border/focus/disabled treatment.
  const editorClass = 'min-h-[24rem] resize-y font-mono text-xs leading-relaxed';

  const isPro = tier === 'pro' || tier === 'enterprise' || tier === 'hosted';

  return (
    <div className="grid gap-6 lg:grid-cols-2">
      {/* Input column */}
      <section className="space-y-3">
        <div className="flex items-center justify-between gap-2">
          <label htmlFor="snapshot" className="text-sm font-medium">
            Metrics &amp; estimate snapshot
          </label>
          <button
            type="button"
            onClick={() => setSnapshot(EXAMPLE_SNAPSHOT)}
            className="text-xs text-muted-foreground underline-offset-2 hover:underline"
          >
            Reset to example
          </button>
        </div>
        <Textarea
          id="snapshot"
          value={snapshot}
          onChange={(e) => setSnapshot(e.target.value)}
          spellCheck={false}
          className={editorClass}
          aria-label="Measured metrics and workload estimate snapshot (JSON)"
        />
        <div className="flex flex-wrap items-center gap-2">
          <Button onClick={run} disabled={pending}>
            {pending ? 'Diagnosing…' : 'Diagnose'}
          </Button>
          <span className="text-xs text-muted-foreground">
            Advisory only - recommendations are never auto-applied.
          </span>
        </div>
      </section>

      {/* Results column */}
      <section className="space-y-3">
        <h2 className="text-sm font-medium">Recommendations</h2>

        {result.kind === 'idle' ? (
          <p className="text-sm text-muted-foreground">
            {pending
              ? 'Running the diagnosis…'
              : 'Diagnose the snapshot to see advisory tuning recommendations with their evidence and confidence.'}
          </p>
        ) : null}

        {result.kind === 'error' ? (
          <ErrorAlert message={result.message} upgrade={result.upgrade} />
        ) : null}

        {result.kind === 'report' ? (
          <ReportPanel report={result.report} isPro={isPro} />
        ) : null}
      </section>
    </div>
  );
}

function ReportPanel({ report, isPro }: { report: TuningReport; isPro: boolean }) {
  if (report.recommendations.length === 0) {
    // The API returns an empty report both for a healthy stack and for a tier
    // that unlocks none of the advisory recommendations. Tailor the empty state.
    return (
      <Card>
        <CardContent>
          <EmptyState
            title={
              <>
                No advisory recommendations were returned for{' '}
                <code className="font-mono text-xs">{report.deployment}</code>.
              </>
            }
            description={
              isPro
                ? 'On this snapshot the engine found no bottleneck to flag.'
                : 'Community plans see basic monitoring only. Advisory tuning recommendations are a Pro feature.'
            }
            action={
              isPro ? undefined : (
                <Button asChild size="sm" variant="outline">
                  <Link href="/pricing">See plans</Link>
                </Button>
              )
            }
          />
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-4">
      <p className="text-xs text-muted-foreground">
        {report.recommendations.length} recommendation
        {report.recommendations.length === 1 ? '' : 's'} for{' '}
        <code className="font-mono">{report.deployment}</code>, in severity order.
      </p>
      <ul className="space-y-4">
        {report.recommendations.map((rec, i) => (
          <li key={`${rec.bottleneck}-${i}`}>
            <RecommendationCard rec={rec} />
          </li>
        ))}
      </ul>
    </div>
  );
}
