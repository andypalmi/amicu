'use client';

import { useCallback, useEffect, useState } from 'react';
import {
  Badge,
  Button,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { EmptyState } from '@/components/common/empty-state';
import { LoadingState } from '@/components/common/loading-state';
import { ConfigDetail } from './detail';
import {
  errorMessage,
  looksLikeConfigSummaryList,
  type ConfigSummaryDto,
} from '@/lib/configs/format';

// The "My Configs" console. Lists every configuration saved from the wizard
// (real data from the Management API through the /api/configs/* BFF) and, once
// one is selected, hands off to <ConfigDetail> for its version history, diff,
// restore, and load-into-wizard actions. There is no create-here form: the sole
// creation path is the wizard's Save action (see components/wizard/hcl-result.tsx)
// - this console only reads and manages what has already been saved.

type ListState =
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; configs: ConfigSummaryDto[] };

export function ConfigsConsole() {
  const [list, setList] = useState<ListState>({ kind: 'loading' });
  const [selectedId, setSelectedId] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const res = await fetch('/api/configs');
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok && looksLikeConfigSummaryList(payload)) {
        setList({ kind: 'ready', configs: payload });
        return;
      }
      setList({ kind: 'error', message: errorMessage(res.status, payload) });
    } catch {
      setList({ kind: 'error', message: 'The configuration service is unreachable.' });
    }
  }, []);

  useEffect(() => {
    void (async () => {
      await load();
    })();
  }, [load]);

  if (selectedId) {
    return (
      <ConfigDetail
        key={selectedId}
        configId={selectedId}
        onBack={() => setSelectedId(null)}
        onChanged={() => void load()}
      />
    );
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Saved configurations</CardTitle>
        <CardDescription>
          Configurations you have saved from the wizard. Select one to review its version history,
          diff two versions, restore an older version, or load it back into the wizard.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {list.kind === 'loading' ? <LoadingState label="Loading configurations…" /> : null}
        {list.kind === 'error' ? <ErrorAlert message={list.message} /> : null}

        {list.kind === 'ready' && list.configs.length === 0 ? (
          <EmptyState
            title="No saved configurations yet"
            description="Emit a configuration in the wizard and use Save to see it here."
          />
        ) : null}

        {list.kind === 'ready' && list.configs.length > 0 ? (
          <Table>
            <TableHeader>
              <TableRow>
                <TableHead>Name</TableHead>
                <TableHead>Version</TableHead>
                <TableHead>Updated</TableHead>
                <TableHead className="text-right">Actions</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {list.configs.map((c) => (
                <TableRow key={c.id}>
                  <TableCell className="font-medium">{c.name}</TableCell>
                  <TableCell>
                    <Badge variant="outline">v{c.current_version}</Badge>
                  </TableCell>
                  <TableCell className="text-xs text-muted-foreground">
                    {formatWhen(c.updated_at)}
                  </TableCell>
                  <TableCell className="text-right">
                    <Button size="sm" variant="outline" onClick={() => setSelectedId(c.id)}>
                      View history
                    </Button>
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        ) : null}
      </CardContent>
    </Card>
  );
}

/** Format a Unix-seconds timestamp for display. Runs only in the browser, after
 *  the config list is fetched client-side - never during prerender. */
function formatWhen(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleString();
}
