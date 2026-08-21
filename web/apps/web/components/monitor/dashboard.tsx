'use client';

import { useState } from 'react';
import Link from 'next/link';
import {
  Badge,
  Button,
  Card,
  CardContent,
  CardHeader,
  CardTitle,
  Separator,
  Textarea,
} from '@amicu/ui';
import { EXAMPLE_SNAPSHOT } from '@/lib/monitor/example';
import {
  looksLikeMonitorView,
  distributionPoints,
  estimateAssumptions,
  formatMeasure,
  formatNumber,
  formatPercent,
  formatMoney,
  formatRange,
  provenanceLabel,
  type MonitorView,
  type StackMetrics,
  type WorkloadEstimate,
  type Distribution,
  type EstimateRange,
  type TuningReport,
} from '@/lib/monitor/format';
import { confidenceMeta, isTierDenied } from '@/lib/recommend/format';
import { useTier } from '@/components/session-provider';
import { ErrorAlert } from '@/components/common/error-alert';
import { EmptyState } from '@/components/common/empty-state';
import { RecommendationCard } from '@/components/recommend/recommendation-card';

type Result =
  | { kind: 'idle' }
  | { kind: 'view'; view: MonitorView }
  | { kind: 'error'; message: string; upgrade?: boolean };

// The monitoring dashboard (measured-vs-estimated): paste a measured metrics +
// estimate snapshot, POST it to the /api/monitor BFF, and render the returned
// metric surface next to the estimate and the engine's advisory suggestions. It
// renders only — every distribution, range, and suggestion comes from the API;
// this component computes no divergence verdict.
export function MonitorDashboard() {
  const tier = useTier();
  const [snapshot, setSnapshot] = useState(EXAMPLE_SNAPSHOT);
  const [pending, setPending] = useState(false);
  const [result, setResult] = useState<Result>({ kind: 'idle' });

  async function run() {
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
      const res = await fetch('/api/monitor', {
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
          message: 'Monitoring requires a higher plan. The endpoint is gated by the API.',
          upgrade: true,
        });
        return;
      }
      if (res.ok && looksLikeMonitorView(payload)) {
        setResult({ kind: 'view', view: payload });
        return;
      }
      if (res.status === 422) {
        setResult({
          kind: 'error',
          message:
            'The request body was malformed (a field had the wrong type). Check the metrics and estimate JSON.',
        });
        return;
      }
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
    <div className="space-y-6">
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
            {pending ? 'Loading…' : 'Show dashboard'}
          </Button>
          <span className="text-xs text-muted-foreground">
            Renders only — the measured distributions are shown beside the estimate; nothing is
            recomputed.
          </span>
        </div>
      </section>

      {result.kind === 'idle' ? (
        <p className="text-sm text-muted-foreground">
          {pending
            ? 'Loading the monitoring view…'
            : 'Load the snapshot to see the metric surface beside the estimate, with the engine’s advisory suggestions.'}
        </p>
      ) : null}

      {result.kind === 'error' ? (
        <ErrorAlert message={result.message} upgrade={result.upgrade} />
      ) : null}

      {result.kind === 'view' ? <MonitorViewPanel view={result.view} isPro={isPro} /> : null}
    </div>
  );
}

function MonitorViewPanel({ view, isPro }: { view: MonitorView; isPro: boolean }) {
  return (
    <div className="space-y-6">
      <p className="text-xs text-muted-foreground">
        Measured surface for{' '}
        <code className="font-mono">{view.metrics.deployment}</code> over a{' '}
        {formatNumber(view.metrics.window_seconds)}s window, shown against its workload estimate.
      </p>

      <LatencyVsEstimate metrics={view.metrics} estimate={view.estimate} />
      <MetricSurface metrics={view.metrics} />
      <SuggestionsPanel report={view.suggestions} isPro={isPro} />
    </div>
  );
}

// ---------------------------------------------------------------------------
// Measured-vs-estimated (the only place an estimate range exists)
// ---------------------------------------------------------------------------

function LatencyVsEstimate({
  metrics,
  estimate,
}: {
  metrics: StackMetrics;
  estimate: WorkloadEstimate;
}) {
  const assumptions = estimateAssumptions(estimate);
  return (
    <MetricCard title="Latency — measured vs. estimated">
      <div className="grid gap-4 md:grid-cols-2">
        <MeasuredVsEstimated
          label="TTFT"
          unit="ms"
          measured={metrics.latency.ttft_ms}
          estimate={estimate.expected_ttft_ms}
        />
        <MeasuredVsEstimated
          label="TPOT"
          unit="ms"
          measured={metrics.latency.tpot_ms}
          estimate={estimate.expected_tpot_ms}
        />
      </div>

      <div className="space-y-1.5">
        <Separator />
        <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          End-to-end latency (measured, no estimate)
        </h3>
        <DistributionRow dist={metrics.latency.e2e_ms} unit="ms" />
      </div>

      <div className="space-y-1.5">
        <Separator />
        <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
          Estimate assumptions
        </h3>
        <ul className="list-inside list-disc space-y-1 text-xs text-muted-foreground">
          {assumptions.map((a, i) => (
            <li key={i}>{a}</li>
          ))}
        </ul>
        <p className="text-xs text-muted-foreground">
          The estimate is a range with a stated confidence, not a single target. A measured value
          outside the band is attributable to these assumptions — read them before drawing a
          conclusion.
        </p>
      </div>
    </MetricCard>
  );
}

function MeasuredVsEstimated({
  label,
  unit,
  measured,
  estimate,
}: {
  label: string;
  unit: string;
  measured: Distribution;
  estimate: EstimateRange;
}) {
  const band = confidenceMeta(estimate.confidence);
  return (
    <Card className="text-sm">
      <CardContent className="space-y-2 p-3">
        <div className="flex items-center justify-between gap-2">
          <span className="font-medium">{label}</span>
          <span className="text-xs text-muted-foreground">{unit}</span>
        </div>

        <div className="space-y-1">
          <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Measured
          </span>
          <DistributionRow dist={measured} unit={unit} />
        </div>

        <div className="space-y-1">
          <span className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Estimated
          </span>
          <div className="flex flex-wrap items-center gap-2">
            <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-[11px]">
              {formatRange(estimate, unit)}
            </code>
            <Badge variant={band.tone}>{band.label}</Badge>
            <span className="text-xs text-muted-foreground">
              {provenanceLabel(estimate.provenance)}
            </span>
          </div>
        </div>
      </CardContent>
    </Card>
  );
}

// ---------------------------------------------------------------------------
// The rest of the metric surface (measured distributions / scalars)
// ---------------------------------------------------------------------------

function MetricSurface({ metrics }: { metrics: StackMetrics }) {
  const { throughput, concurrency, queue, memory, gpu, cost, reliability } = metrics;
  return (
    <div className="grid gap-4 md:grid-cols-2">
      <MetricCard title="Throughput">
        <ScalarRow label="Output tokens/s" value={formatMeasure(throughput.output_tokens_per_s)} />
        <ScalarRow label="Prompt tokens/s" value={formatMeasure(throughput.prompt_tokens_per_s)} />
        <ScalarRow label="Requests/s" value={formatMeasure(throughput.requests_per_s)} />
      </MetricCard>

      <MetricCard title="Concurrency & batching">
        <ScalarRow label="In-flight requests" value={formatNumber(concurrency.in_flight_requests)} />
        <ScalarRow label="Running batch size" value={formatNumber(concurrency.running_batch_size)} />
        <ScalarRow label="max-num-seqs" value={formatNumber(concurrency.max_num_seqs)} />
        <ScalarRow label="Seq saturation" value={formatPercent(concurrency.seq_saturation)} />
      </MetricCard>

      <MetricCard title="Queue">
        <ScalarRow label="Depth" value={formatNumber(queue.depth)} />
        <div className="space-y-1">
          <span className="text-xs text-muted-foreground">Wait time</span>
          <DistributionRow dist={queue.wait_ms} unit="ms" />
        </div>
        <ScalarRow label="Admitted" value={formatNumber(queue.admitted)} />
        <ScalarRow label="Rejected" value={formatNumber(queue.rejected)} />
      </MetricCard>

      <MetricCard title="Memory / KV cache">
        <ScalarRow label="KV utilization" value={formatPercent(memory.kv_cache_utilization)} />
        <ScalarRow label="Preemptions" value={formatNumber(memory.preemptions)} />
        <ScalarRow label="Recomputations" value={formatNumber(memory.recomputations)} />
        <ScalarRow label="OOM events" value={formatNumber(memory.oom_events)} />
      </MetricCard>

      <MetricCard title="GPU">
        <ScalarRow label="SM utilization" value={formatPercent(gpu.sm_utilization)} />
        <ScalarRow label="VRAM used" value={gpu.vram_used} />
        <ScalarRow label="Power" value={formatMeasure(gpu.power_watts, 'W')} />
      </MetricCard>

      <MetricCard title="Cost">
        <ScalarRow label="Per hour" value={formatMoney(cost.per_hour)} />
        <ScalarRow label="Per 1M tokens" value={formatMoney(cost.per_million_tokens)} />
        <ScalarRow label="Per request" value={formatMoney(cost.per_request)} />
        <ScalarRow
          label="Utilization-adjusted / hour"
          value={formatMoney(cost.utilization_adjusted_per_hour)}
        />
      </MetricCard>

      <MetricCard title="Reliability">
        <ScalarRow label="Error rate" value={formatPercent(reliability.error_rate)} />
        <ScalarRow label="Timeout rate" value={formatPercent(reliability.timeout_rate)} />
        <ScalarRow label="Restarts" value={formatNumber(reliability.restarts)} />
      </MetricCard>
    </div>
  );
}

// A titled metric card composed from the kit `Card` primitives. This replaces a
// former local `Card` component that shadowed `@amicu/ui`'s `Card`; it keeps the
// console's compact p-4 sizing and section heading exactly as before.
function MetricCard({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <Card className="text-sm">
      <CardHeader className="p-4 pb-0">
        <CardTitle className="text-sm font-semibold">{title}</CardTitle>
      </CardHeader>
      <CardContent className="space-y-3 p-4 pt-3">{children}</CardContent>
    </Card>
  );
}

function ScalarRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="text-muted-foreground">{label}</span>
      <span className="font-mono text-xs tabular-nums">{value}</span>
    </div>
  );
}

// A distribution shown as its three percentiles — never a single point as truth.
function DistributionRow({ dist, unit }: { dist: Distribution; unit?: string }) {
  const points = distributionPoints(dist, unit);
  return (
    <div className="flex flex-wrap gap-2" aria-label="percentile distribution">
      {points.map((p) => (
        <span
          key={p.label}
          className="inline-flex items-baseline gap-1 rounded bg-muted px-1.5 py-0.5"
        >
          <span className="text-[10px] font-medium uppercase text-muted-foreground">{p.label}</span>
          <span className="font-mono text-xs tabular-nums">{p.value}</span>
        </span>
      ))}
    </div>
  );
}

// ---------------------------------------------------------------------------
// Advisory suggestions (the engine's — not automatic — changes)
// ---------------------------------------------------------------------------

function SuggestionsPanel({ report, isPro }: { report: TuningReport; isPro: boolean }) {
  if (report.recommendations.length === 0) {
    return (
      <Card className="text-sm">
        <CardHeader className="p-4 pb-0">
          <CardTitle className="text-sm font-semibold">Suggested changes</CardTitle>
        </CardHeader>
        <CardContent className="p-4 pt-3">
          <EmptyState
            title={
              isPro
                ? 'On this snapshot the engine found no bottleneck to flag.'
                : 'Community plans see the metric surface only. Advisory tuning suggestions are a Pro feature.'
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
    <section className="space-y-3">
      <h2 className="text-sm font-semibold">
        Suggested changes{' '}
        <span className="font-normal text-muted-foreground">
          ({report.recommendations.length} — advisory, never auto-applied)
        </span>
      </h2>
      <ul className="space-y-4">
        {report.recommendations.map((rec, i) => (
          <li key={`${rec.bottleneck}-${i}`}>
            <RecommendationCard rec={rec} />
          </li>
        ))}
      </ul>
    </section>
  );
}
