'use client';

import { useState } from 'react';
import { Button, Textarea } from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { LoadingState } from '@/components/common/loading-state';
import { DiagnosticsPanel } from '@/components/engine/diagnostics-panel';
import { PlanPanel } from '@/components/engine/plan-panel';
import { isTierDenied, type Plan } from '@/lib/engine/format';
import {
  archetypeLabel,
  looksLikeCustomizeResponse,
  type CandidateArchetypeDto,
  type CustomizeResponseDto,
} from '@/lib/recommend/candidates';

/** The subset of a wizard `CandidateDto` this panel needs. */
export type CandidateReviewCandidate = {
  hcl: string;
  /** Opaque canonical JSON graph - forwarded verbatim as `base_graph`. */
  graph: Record<string, never>;
  archetype: CandidateArchetypeDto;
  rationale: string[];
};

type ReviewState =
  | { kind: 'idle' }
  | { kind: 'error'; message: string; upgrade?: boolean }
  | { kind: 'result'; result: CustomizeResponseDto };

// The deterministic "customize" review panel: an
// editable HCL textarea seeded from a wizard candidate, a "Review changes"
// action that re-parses/validates/plans the CURRENT editor text against the
// candidate's original graph via POST /api/recommend/customize, and an
// explicit confirmation gate over the resulting Plan.
//
// The confirmation checkbox (and Confirm button) is enabled ONLY while a valid
// plan is currently displayed for the exact HCL text in the editor - editing
// after a review immediately clears both, so a user can never confirm a plan
// that doesn't match what they're looking at.
export function CandidateReview({
  candidate,
  onConfirm = () => {},
  onDirtyChange,
}: {
  candidate: CandidateReviewCandidate;
  onConfirm?: (hcl: string, plan: Plan) => void;
  /** Called whenever the editor's text starts or stops matching the
   *  candidate's original seed HCL, from the same handler that updates the
   *  textarea state (not an effect, to avoid a setState-in-effect lint and an
   *  extra render). */
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const [hcl, setHcl] = useState(candidate.hcl);
  const [reviewing, setReviewing] = useState(false);
  const [review, setReview] = useState<ReviewState>({ kind: 'idle' });
  // The HCL text the last plan-bearing review ran against. The confirmation
  // gate compares this to the CURRENT editor text, not just "a review ran" -
  // so an edit after reviewing (even one that round-trips back to the same
  // text) requires a fresh review before confirming.
  const [reviewedHcl, setReviewedHcl] = useState<string | null>(null);
  const [confirmed, setConfirmed] = useState(false);

  const plan = review.kind === 'result' ? (review.result.plan ?? null) : null;
  const canConfirm = confirmed && plan !== null && reviewedHcl === hcl;

  function editHcl(next: string) {
    setHcl(next);
    setConfirmed(false);
    onDirtyChange?.(next !== candidate.hcl);
  }

  async function reviewChanges() {
    setReviewing(true);
    setReview({ kind: 'idle' });
    setConfirmed(false);
    try {
      const res = await fetch('/api/recommend/customize', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ hcl, base_graph: candidate.graph }),
      });
      const payload: unknown = await res.json().catch(() => null);

      if (res.status === 401) {
        setReview({
          kind: 'error',
          message: 'Your session has expired. Reload the page or sign in again.',
        });
        return;
      }
      if (isTierDenied(res.status)) {
        setReview({
          kind: 'error',
          message: 'Reviewing changes requires a higher plan. The engine is gated by the API.',
          upgrade: true,
        });
        return;
      }
      // 200 (clean plan) and 422 (blocking diagnostics, no plan) share the same
      // CustomizeResponseDto body shape - both are a "result" to render.
      if ((res.ok || res.status === 422) && looksLikeCustomizeResponse(payload)) {
        setReview({ kind: 'result', result: payload });
        if (payload.plan) setReviewedHcl(hcl);
        return;
      }
      const message =
        (payload as { error?: string })?.error ?? `Could not review the changes (${res.status}).`;
      setReview({ kind: 'error', message });
    } catch {
      setReview({ kind: 'error', message: 'Could not reach the server. Is it running?' });
    } finally {
      setReviewing(false);
    }
  }

  return (
    <div className="space-y-4">
      <div className="space-y-1">
        <h3 className="text-sm font-medium">{archetypeLabel(candidate.archetype)} candidate</h3>
        {candidate.rationale.length > 0 ? (
          <ul className="list-inside list-disc text-xs text-muted-foreground">
            {candidate.rationale.map((reason, i) => (
              <li key={i}>{reason}</li>
            ))}
          </ul>
        ) : null}
      </div>

      <div className="flex flex-wrap items-center justify-between gap-2">
        <label htmlFor="candidate-hcl" className="text-sm font-medium">
          Editable configuration
        </label>
        <Button type="button" size="sm" onClick={() => void reviewChanges()} disabled={reviewing}>
          {reviewing ? 'Reviewing…' : 'Review changes'}
        </Button>
      </div>
      <Textarea
        id="candidate-hcl"
        value={hcl}
        onChange={(e) => editHcl(e.target.value)}
        spellCheck={false}
        className="min-h-[24rem] resize-y font-mono text-xs leading-relaxed"
        aria-label="Editable amicu HCL configuration for this candidate"
      />
      <p className="text-xs text-muted-foreground">
        Edit the HCL, then Review changes to re-validate it and diff it against this
        candidate&apos;s original configuration. Editing after a review clears the confirmation
        below - review again before confirming.
      </p>

      {reviewing ? <LoadingState label="Validating and planning…" /> : null}
      {review.kind === 'error' ? (
        <ErrorAlert message={review.message} upgrade={review.upgrade} />
      ) : null}

      {review.kind === 'result' ? (
        <div className="space-y-4">
          <div className="space-y-2">
            <h4 className="text-sm font-medium">Diagnostics</h4>
            <DiagnosticsPanel report={review.result.report} />
          </div>
          {plan ? (
            <div className="space-y-2">
              <h4 className="text-sm font-medium">Plan</h4>
              <PlanPanel plan={plan} />
            </div>
          ) : null}
        </div>
      ) : null}

      <div className="flex flex-wrap items-center gap-3 rounded-md border border-border p-3">
        <input
          id="candidate-confirm"
          type="checkbox"
          checked={confirmed}
          disabled={plan === null || reviewedHcl !== hcl}
          onChange={(e) => setConfirmed(e.target.checked)}
          className="h-4 w-4"
        />
        <label htmlFor="candidate-confirm" className="text-sm">
          I have reviewed this plan.
        </label>
        <Button
          type="button"
          size="sm"
          disabled={!canConfirm}
          onClick={() => {
            if (plan) onConfirm(hcl, plan);
          }}
        >
          Confirm
        </Button>
        {plan === null ? (
          <span className="text-xs text-muted-foreground">
            Review changes to a valid plan before confirming.
          </span>
        ) : reviewedHcl !== hcl ? (
          <span className="text-xs text-muted-foreground">
            The editor has changed since the last review - review again to confirm.
          </span>
        ) : null}
      </div>
    </div>
  );
}
