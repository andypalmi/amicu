'use client';

import { useState } from 'react';
import { Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Label, Select } from '@amicu/ui';
import { cn } from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { LoadingState } from '@/components/common/loading-state';
import {
  classifyDiffLine,
  errorMessage,
  looksLikeConfigDiff,
  type ConfigDiffDto,
  type VersionMetaDto,
} from '@/lib/configs/format';

type DiffState =
  | { kind: 'idle' }
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; diff: ConfigDiffDto };

// From/to version pickers plus a colored unified diff, rendered via
// GET /api/configs/{id}/diff. The diff text itself comes straight from the
// Management API (a standard unified diff) - this only classifies and colors
// each line for display; it never recomputes the diff client-side.
export function DiffPanel({
  configId,
  versions,
}: {
  configId: string;
  versions: VersionMetaDto[];
}) {
  const sorted = [...versions].sort((a, b) => a.version_no - b.version_no);
  const [from, setFrom] = useState<number>(sorted[sorted.length - 2]?.version_no ?? sorted[0]!.version_no);
  const [to, setTo] = useState<number>(sorted[sorted.length - 1]!.version_no);
  const [state, setState] = useState<DiffState>({ kind: 'idle' });

  if (sorted.length < 2) return null;

  async function runDiff() {
    setState({ kind: 'loading' });
    try {
      const res = await fetch(
        `/api/configs/${encodeURIComponent(configId)}/diff?from=${from}&to=${to}`,
      );
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok && looksLikeConfigDiff(payload)) {
        setState({ kind: 'ready', diff: payload });
        return;
      }
      setState({ kind: 'error', message: errorMessage(res.status, payload) });
    } catch {
      setState({ kind: 'error', message: 'The configuration service is unreachable.' });
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Compare versions</CardTitle>
        <CardDescription>A unified diff between any two saved versions.</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex flex-wrap items-end gap-3">
          <div className="space-y-1">
            <Label htmlFor="diff-from">From</Label>
            <Select
              id="diff-from"
              value={from}
              onChange={(e) => setFrom(Number(e.target.value))}
              className="w-24"
            >
              {sorted.map((v) => (
                <option key={v.version_no} value={v.version_no}>
                  v{v.version_no}
                </option>
              ))}
            </Select>
          </div>
          <div className="space-y-1">
            <Label htmlFor="diff-to">To</Label>
            <Select
              id="diff-to"
              value={to}
              onChange={(e) => setTo(Number(e.target.value))}
              className="w-24"
            >
              {sorted.map((v) => (
                <option key={v.version_no} value={v.version_no}>
                  v{v.version_no}
                </option>
              ))}
            </Select>
          </div>
          <Button size="sm" onClick={() => void runDiff()} disabled={state.kind === 'loading'}>
            {state.kind === 'loading' ? 'Diffing…' : 'Show diff'}
          </Button>
        </div>

        {state.kind === 'error' ? <ErrorAlert message={state.message} /> : null}
        {state.kind === 'loading' ? <LoadingState label="Loading diff…" /> : null}
        {state.kind === 'ready' ? <DiffView diff={state.diff.diff} /> : null}
      </CardContent>
    </Card>
  );
}

function DiffView({ diff }: { diff: string }) {
  const lines = diff.split('\n');
  if (lines.length === 0 || (lines.length === 1 && lines[0] === '')) {
    return <p className="text-sm text-muted-foreground">These versions are identical.</p>;
  }
  return (
    <pre className="max-h-[32rem] overflow-auto rounded-md border border-border bg-muted/30 p-3 font-mono text-xs leading-relaxed">
      {lines.map((line, i) => {
        const kind = classifyDiffLine(line);
        return (
          <div
            key={i}
            className={cn(
              kind === 'add' && 'bg-success/10 text-success',
              kind === 'remove' && 'bg-destructive/10 text-destructive',
              kind === 'hunk' && 'text-muted-foreground',
              kind === 'header' && 'font-medium text-muted-foreground',
            )}
          >
            {line.length === 0 ? ' ' : line}
          </div>
        );
      })}
    </pre>
  );
}
