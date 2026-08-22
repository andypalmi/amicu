'use client';

import { useState } from 'react';
import { Button, Card, CardContent, Textarea } from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { DiagnosticsPanel } from '@/components/engine/diagnostics-panel';
import { isTierDenied, type DiagnosticReport } from '@/lib/engine/format';
import { downloadText } from '@/lib/wizard/download';
import { formatRangeMs, type EmitResponseDto } from '@/lib/wizard/format';

const DOWNLOAD_NAME = 'stack.amicu.hcl';

type Validation =
  | { kind: 'idle' }
  | { kind: 'report'; report: DiagnosticReport }
  | { kind: 'error'; message: string; upgrade?: boolean };

function looksLikeReport(body: unknown): body is DiagnosticReport {
  return (
    !!body &&
    typeof body === 'object' &&
    Array.isArray((body as { diagnostics?: unknown }).diagnostics)
  );
}

// The wizard's final panel: the emitted HCL in an EDITABLE editor, plus Copy /
// Download / Validate actions and the preset's estimate for context. Validate
// posts the CURRENT editor text (edits included) to the /api/engine/validate BFF
// and reuses the engine console's DiagnosticsPanel to render the result. The
// estimate is the preset's own default_estimate, carried through for display -
// the emit path synthesizes no metrics.
export function HclResult({ result }: { result: EmitResponseDto }) {
  const [hcl, setHcl] = useState(result.hcl);
  const [copied, setCopied] = useState(false);
  const [validating, setValidating] = useState(false);
  const [validation, setValidation] = useState<Validation>({ kind: 'idle' });

  const estimate = result.estimate;

  async function copy() {
    try {
      await navigator.clipboard.writeText(hcl);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Clipboard can be blocked (permissions/insecure context); fail quietly.
      setCopied(false);
    }
  }

  async function validate() {
    setValidating(true);
    setValidation({ kind: 'idle' });
    try {
      const res = await fetch('/api/engine/validate', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ config: hcl }),
      });
      const payload: unknown = await res.json().catch(() => null);

      if (res.status === 401) {
        setValidation({
          kind: 'error',
          message: 'Your session has expired. Reload the page or sign in again.',
        });
        return;
      }
      if (isTierDenied(res.status)) {
        setValidation({
          kind: 'error',
          message: 'Validation requires a higher plan. The engine is gated by the API.',
          upgrade: true,
        });
        return;
      }
      if (res.ok && looksLikeReport(payload)) {
        setValidation({ kind: 'report', report: payload });
        return;
      }
      const message =
        (payload as { error?: string })?.error ?? `Validation failed (${res.status}).`;
      setValidation({ kind: 'error', message });
    } catch {
      setValidation({ kind: 'error', message: 'Could not reach the server. Is it running?' });
    } finally {
      setValidating(false);
    }
  }

  const editorClass = 'min-h-[24rem] resize-y font-mono text-xs leading-relaxed';

  return (
    <div className="space-y-4">
      <div className="flex items-center justify-between gap-2">
        <label htmlFor="hcl" className="text-sm font-medium">
          Generated configuration
        </label>
        <div className="flex flex-wrap items-center gap-2">
          <Button type="button" variant="outline" size="sm" onClick={copy}>
            {copied ? 'Copied' : 'Copy'}
          </Button>
          <Button
            type="button"
            variant="outline"
            size="sm"
            onClick={() => downloadText(DOWNLOAD_NAME, hcl)}
          >
            Download
          </Button>
          <Button type="button" size="sm" onClick={validate} disabled={validating}>
            {validating ? 'Validating…' : 'Validate'}
          </Button>
        </div>
      </div>

      <Textarea
        id="hcl"
        value={hcl}
        onChange={(e) => setHcl(e.target.value)}
        spellCheck={false}
        className={editorClass}
        aria-label="Generated Amicu HCL configuration"
      />
      <p className="text-xs text-muted-foreground">
        Editable - your edits are what Copy, Download, and Validate use. Downloads as{' '}
        <code className="font-mono">{DOWNLOAD_NAME}</code>.
      </p>

      {/* Expected serving behavior, carried through from the preset for context. */}
      <Card>
        <CardContent className="space-y-2 p-4">
          <h3 className="text-sm font-medium">Expected performance</h3>
          <p className="text-xs text-muted-foreground">
            The preset&apos;s own estimate, shown for context. No metrics were measured or
            synthesized to produce it.
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
          <p className="text-xs text-muted-foreground">
            Assumes {estimate.assumed_concurrency} concurrent request
            {estimate.assumed_concurrency === 1 ? '' : 's'}, {estimate.assumed_prompt_tokens} prompt
            and {estimate.assumed_generation_tokens} generation tokens.
          </p>
        </CardContent>
      </Card>

      {/* Validation result, rendered with the shared engine diagnostics panel. */}
      {validation.kind === 'error' ? (
        <ErrorAlert message={validation.message} upgrade={validation.upgrade} />
      ) : null}
      {validation.kind === 'report' ? (
        <div className="space-y-2">
          <h3 className="text-sm font-medium">Validation</h3>
          <DiagnosticsPanel report={validation.report} />
        </div>
      ) : null}
    </div>
  );
}
