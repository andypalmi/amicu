'use client';

import { useState } from 'react';
import Link from 'next/link';
import { Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Input, Label, Textarea } from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { DiagnosticsPanel } from '@/components/engine/diagnostics-panel';
import { isTierDenied, type DiagnosticReport } from '@/lib/engine/format';
import { downloadText } from '@/lib/wizard/download';
import { formatRangeMs, type WorkloadEstimateDto } from '@/lib/wizard/format';
import {
  errorMessage,
  looksLikeConfigDetail,
  looksLikeVersionMeta,
} from '@/lib/configs/format';
import { toast } from '@/lib/notify/toast';

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
        aria-label="Generated Amicu HCL configuration"
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

type SaveMode = 'existing' | 'new';

// Inline "Save this configuration" form. Persists the CURRENT editor text (via
// the parent's `hcl` prop) through the /api/configs/* BFF: either as a new
// version of an existing config (POST .../versions) or as a brand new config
// (POST /api/configs). When loaded from an existing config (`sourceConfigId`),
// "new version" is the default; either way, a successful save re-targets this
// form at the resulting config, so a second Save appends another version
// instead of failing on a duplicate name.
function SaveConfigForm({
  hcl,
  sourceConfigId,
  sourceConfigName,
  onSaved,
}: {
  hcl: string;
  sourceConfigId?: string;
  sourceConfigName?: string;
  onSaved?: (config: { id: string; name: string }) => void;
}) {
  const [target, setTarget] = useState<{ id: string; name: string } | null>(
    sourceConfigId && sourceConfigName ? { id: sourceConfigId, name: sourceConfigName } : null,
  );
  const [mode, setMode] = useState<SaveMode>(target ? 'existing' : 'new');
  const [name, setName] = useState('');
  const [message, setMessage] = useState('');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [savedVersion, setSavedVersion] = useState<number | null>(null);

  async function save() {
    setSaving(true);
    setError(null);
    setSavedVersion(null);
    try {
      if (mode === 'existing' && target) {
        const res = await fetch(`/api/configs/${encodeURIComponent(target.id)}/versions`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ content: hcl, message: message.trim() || undefined }),
        });
        const payload: unknown = await res.json().catch(() => null);
        if (res.ok && looksLikeVersionMeta(payload)) {
          toast.success(`Saved as v${payload.version_no} of "${target.name}"`);
          setSavedVersion(payload.version_no);
          setMessage('');
          onSaved?.(target);
          return;
        }
        setError(errorMessage(res.status, payload));
        return;
      }

      if (name.trim() === '') {
        setError('Name is required.');
        return;
      }
      const res = await fetch('/api/configs', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name: name.trim(),
          content: hcl,
          message: message.trim() || undefined,
        }),
      });
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok && looksLikeConfigDetail(payload)) {
        toast.success(`Saved as "${payload.name}"`);
        setSavedVersion(payload.current_version);
        const saved = { id: payload.id, name: payload.name };
        setTarget(saved);
        setMode('existing');
        setName('');
        setMessage('');
        onSaved?.(saved);
        return;
      }
      setError(errorMessage(res.status, payload));
    } catch {
      setError('The configuration service is unreachable.');
    } finally {
      setSaving(false);
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Save this configuration</CardTitle>
        <CardDescription>
          {target
            ? `Persist the editor's current text to My Configs, appending a new version to "${target.name}" unless you save it as a different config.`
            : "Persist the editor's current text to My Configs under a name of your choice."}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {target ? (
          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              size="sm"
              variant={mode === 'existing' ? 'default' : 'outline'}
              aria-pressed={mode === 'existing'}
              onClick={() => setMode('existing')}
            >
              New version of &quot;{target.name}&quot;
            </Button>
            <Button
              type="button"
              size="sm"
              variant={mode === 'new' ? 'default' : 'outline'}
              aria-pressed={mode === 'new'}
              onClick={() => setMode('new')}
            >
              Save as a different config
            </Button>
          </div>
        ) : null}

        <div className="flex flex-wrap items-end gap-3">
          {mode === 'new' ? (
            <div className="space-y-1">
              <Label htmlFor="save-name">Name</Label>
              <Input
                id="save-name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="e.g. Production inference"
                className="w-56"
              />
            </div>
          ) : null}
          <div className="space-y-1">
            <Label htmlFor="save-message">Message (optional)</Label>
            <Input
              id="save-message"
              value={message}
              onChange={(e) => setMessage(e.target.value)}
              placeholder="What changed?"
              className="w-64"
            />
          </div>
          <Button
            type="button"
            size="sm"
            onClick={() => void save()}
            disabled={saving || (mode === 'new' && name.trim() === '')}
          >
            {saving ? 'Saving…' : 'Save'}
          </Button>
        </div>

        {error ? <ErrorAlert message={error} /> : null}
        {savedVersion !== null ? (
          <p className="text-xs text-muted-foreground">
            Saved as version {savedVersion}. Find it any time under{' '}
            <Link href="/configs" className="underline underline-offset-2">
              My Configs
            </Link>
            .
          </p>
        ) : null}
      </CardContent>
    </Card>
  );
}
