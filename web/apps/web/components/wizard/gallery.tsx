'use client';

import { useEffect, useState } from 'react';
import {
  Badge,
  Button,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { EmptyState } from '@/components/common/empty-state';
import { LoadingState } from '@/components/common/loading-state';
import {
  constraintLabel,
  tierLabel,
  type PresetSummaryDto,
} from '@/lib/wizard/format';

type State =
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; presets: PresetSummaryDto[] };

function looksLikeSummaryList(body: unknown): body is PresetSummaryDto[] {
  return (
    Array.isArray(body) &&
    body.every(
      (p) => !!p && typeof p === 'object' && typeof (p as { id?: unknown }).id === 'string',
    )
  );
}

// The preset gallery: fetches GET /api/presets and renders each catalog entry as
// a card with its constraints (as badges) and a "Use this preset" action. The
// catalog is Community-minimum; the min-tier badge is informational only (real
// gating is the API's job). `selectedId` highlights the active card when the
// gallery is embedded in the wizard stepper.
export function Gallery({
  onSelect,
  selectedId,
}: {
  onSelect: (preset: PresetSummaryDto) => void;
  selectedId?: string;
}) {
  const [state, setState] = useState<State>({ kind: 'loading' });

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch('/api/presets');
        const payload: unknown = await res.json().catch(() => null);
        if (cancelled) return;
        if (res.status === 401) {
          setState({
            kind: 'error',
            message: 'Your session has expired. Reload the page or sign in again.',
          });
          return;
        }
        if (res.ok && looksLikeSummaryList(payload)) {
          setState({ kind: 'ready', presets: payload });
          return;
        }
        const message =
          (payload as { error?: string })?.error ?? `Could not load presets (${res.status}).`;
        setState({ kind: 'error', message });
      } catch {
        if (!cancelled) {
          setState({ kind: 'error', message: 'Could not reach the server. Is it running?' });
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  if (state.kind === 'loading') return <LoadingState label="Loading presets…" />;
  if (state.kind === 'error') return <ErrorAlert message={state.message} />;

  if (state.presets.length === 0) {
    return (
      <EmptyState
        title="No presets are available"
        description="The catalog came back empty. Try reloading the page."
      />
    );
  }

  return (
    <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
      {state.presets.map((preset) => {
        const active = preset.id === selectedId;
        return (
          <Card
            key={preset.id}
            className={active ? 'border-foreground/40 ring-1 ring-foreground/20' : undefined}
          >
            <CardHeader>
              <div className="flex items-start justify-between gap-2">
                <CardTitle>{preset.name}</CardTitle>
                <Badge variant="outline" title={`Minimum plan: ${tierLabel(preset.min_tier)}`}>
                  {tierLabel(preset.min_tier)}
                </Badge>
              </div>
              <CardDescription>{preset.tagline}</CardDescription>
            </CardHeader>
            <CardContent className="space-y-4">
              <p className="text-sm text-muted-foreground">{preset.fit}</p>
              {preset.constraints.length > 0 ? (
                <div className="flex flex-wrap gap-1.5">
                  {preset.constraints.map((constraint, i) => (
                    <Badge key={`${preset.id}-c-${i}`} variant="secondary">
                      {constraintLabel(constraint)}
                    </Badge>
                  ))}
                </div>
              ) : null}
              <Button
                onClick={() => onSelect(preset)}
                variant={active ? 'default' : 'outline'}
                size="sm"
                className="w-full"
              >
                {active ? 'Selected' : 'Use this preset'}
              </Button>
            </CardContent>
          </Card>
        );
      })}
    </div>
  );
}
