'use client';

import { useState } from 'react';
import { Button, Card, CardContent, Textarea } from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { DiagnosticsPanel } from '@/components/engine/diagnostics-panel';
import { SaveConfigForm } from '@/components/configs/save-config-form';
import { isTierDenied, type DiagnosticReport } from '@/lib/engine/format';
import { downloadText } from '@/lib/wizard/download';
import { formatRangeMs, type WorkloadEstimateDto } from '@/lib/wizard/format';

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

// The result this panel renders: either a freshly-emitted preset result (with an
// estimate) or a configuration loaded from "My Configs" (no estimate - it was
// never synthesized here). `estimate` is therefore optional and its card is only
// shown when present.
export type HclResultData = {
  hcl: string;
  estimate?: WorkloadEstimateDto;
};

// The wizard's final panel: the emitted (or loaded) HCL in an EDITABLE editor,
// plus Copy / Download / Validate / Save actions and, when present, the preset's
// estimate for context. Validate posts the CURRENT editor text (edits included)
// to the /api/engine/validate BFF and reuses the engine console's
// DiagnosticsPanel to render the result. Save persists the CURRENT editor text
// through the /api/configs/* BFF - see the `<SaveConfigForm>` below. Neither
// action synthesizes anything the API did not itself return.
export function HclResult({
  result,
  sourceConfigId,
  sourceConfigName,
  onSaved,
}: {
  result: HclResultData;
  /** Set when this HCL was loaded from an existing "My Configs" entry, so Save
   *  can default to appending a new version to it instead of creating a new
   *  config. */
  sourceConfigId?: string;
  sourceConfigName?: string;
  onSaved?: (config: { id: string; name: string }) => void;
}) {
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
        aria-label="Generated amicu HCL configuration"
      />
      <p className="text-xs text-muted-foreground">
        Editable - your edits are what Copy, Download, and Validate use. Downloads as{' '}
        <code className="font-mono">{DOWNLOAD_NAME}</code>.
      </p>

      {/* Expected serving behavior, carried through from the preset for context.
          Absent when this HCL was loaded from My Configs rather than freshly
          emitted - nothing is synthesized to fill the gap. */}
      {estimate ? (
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
      ) : null}

      {/* Persist the current editor text through the /api/configs/* BFF. */}
      <SaveConfigForm hcl={hcl} sourceConfigId={sourceConfigId} sourceConfigName={sourceConfigName} onSaved={onSaved} />

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
