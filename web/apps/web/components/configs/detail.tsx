'use client';

import { useCallback, useEffect, useState } from 'react';
import Link from 'next/link';
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
import { LoadingState } from '@/components/common/loading-state';
import { toast } from '@/lib/notify/toast';
import { DiffPanel } from './diff-panel';
import {
  errorMessage,
  looksLikeConfigDetail,
  looksLikeVersionMeta,
  looksLikeVersionMetaList,
  shortSha,
  type ConfigDetailDto,
  type VersionMetaDto,
} from '@/lib/configs/format';

type DetailLoad =
  | { kind: 'loading' }
  | { kind: 'error'; message: string }
  | { kind: 'ready'; detail: ConfigDetailDto; versions: VersionMetaDto[] };

// One configuration's detail: current version, full version history, restore
// (rollback creates a NEW version - nothing is deleted or overwritten), and a
// "Load into wizard" link that seeds the wizard from this config's current
// content (see components/wizard/wizard.tsx's `?configId=` handling). Reads
// through GET /api/configs/{id} and GET /api/configs/{id}/versions; restore
// through POST /api/configs/{id}/rollback/{no}.
export function ConfigDetail({
  configId,
  onBack,
  onChanged,
}: {
  configId: string;
  onBack: () => void;
  onChanged: () => void;
}) {
  const [load, setLoad] = useState<DetailLoad>({ kind: 'loading' });
  const [restoring, setRestoring] = useState<number | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      const [detailRes, versionsRes] = await Promise.all([
        fetch(`/api/configs/${encodeURIComponent(configId)}`),
        fetch(`/api/configs/${encodeURIComponent(configId)}/versions`),
      ]);
      const detailBody: unknown = await detailRes.json().catch(() => null);
      if (!detailRes.ok || !looksLikeConfigDetail(detailBody)) {
        setLoad({ kind: 'error', message: errorMessage(detailRes.status, detailBody) });
        return;
      }
      const versionsBody: unknown = await versionsRes.json().catch(() => null);
      if (!versionsRes.ok || !looksLikeVersionMetaList(versionsBody)) {
        setLoad({ kind: 'error', message: errorMessage(versionsRes.status, versionsBody) });
        return;
      }
      // Most-recent first for display.
      const versions = [...versionsBody].sort((a, b) => b.version_no - a.version_no);
      setLoad({ kind: 'ready', detail: detailBody, versions });
    } catch {
      setLoad({ kind: 'error', message: 'The configuration service is unreachable.' });
    }
  }, [configId]);

  useEffect(() => {
    void (async () => {
      await refresh();
    })();
  }, [refresh]);

  async function restore(versionNo: number) {
    setRestoring(versionNo);
    setActionError(null);
    try {
      const res = await fetch(
        `/api/configs/${encodeURIComponent(configId)}/rollback/${versionNo}`,
        { method: 'POST' },
      );
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok && looksLikeVersionMeta(payload)) {
        toast.success(`Restored v${versionNo} as new version v${payload.version_no}`);
        await refresh();
        onChanged();
        return;
      }
      setActionError(errorMessage(res.status, payload));
    } catch {
      setActionError('The configuration service is unreachable.');
    } finally {
      setRestoring(null);
    }
  }

  return (
    <div className="space-y-6">
      <Button variant="outline" size="sm" onClick={onBack}>
        ← Back to configurations
      </Button>

      {load.kind === 'loading' ? <LoadingState label="Loading configuration…" /> : null}
      {load.kind === 'error' ? <ErrorAlert message={load.message} /> : null}

      {load.kind === 'ready' ? (
        <>
          <Card>
            <CardHeader>
              <div className="flex flex-wrap items-start justify-between gap-3">
                <div>
                  <CardTitle className="text-base">{load.detail.name}</CardTitle>
                  <CardDescription>
                    Current version <Badge variant="outline">v{load.detail.current_version}</Badge>
                    {' · '}updated {formatWhen(load.detail.updated_at)}
                  </CardDescription>
                </div>
                <Button asChild size="sm">
                  <Link href={`/wizard?configId=${encodeURIComponent(load.detail.id)}`}>
                    Load into wizard
                  </Link>
                </Button>
              </div>
            </CardHeader>
          </Card>

          {actionError ? <ErrorAlert message={actionError} /> : null}

          <Card>
            <CardHeader>
              <CardTitle className="text-base">Version history</CardTitle>
              <CardDescription>
                Every saved version, most-recent first. Restoring an older version creates a new
                version with that version&apos;s content.
              </CardDescription>
            </CardHeader>
            <CardContent>
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead>Version</TableHead>
                    <TableHead>Created</TableHead>
                    <TableHead>Message</TableHead>
                    <TableHead>Content</TableHead>
                    <TableHead className="text-right">Actions</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {load.versions.map((v) => {
                    const isCurrent = v.version_no === load.detail.current_version;
                    return (
                      <TableRow key={v.version_no}>
                        <TableCell>
                          <Badge variant={isCurrent ? 'default' : 'outline'}>v{v.version_no}</Badge>
                        </TableCell>
                        <TableCell className="text-xs text-muted-foreground">
                          {formatWhen(v.created_at)}
                        </TableCell>
                        <TableCell className="text-sm">
                          {v.message ? v.message : <span className="text-muted-foreground">—</span>}
                        </TableCell>
                        <TableCell className="font-mono text-xs text-muted-foreground">
                          {shortSha(v.content_sha256)}
                        </TableCell>
                        <TableCell className="text-right">
                          <Button
                            size="sm"
                            variant="outline"
                            disabled={restoring !== null || isCurrent}
                            onClick={() => void restore(v.version_no)}
                          >
                            {restoring === v.version_no ? 'Restoring…' : 'Restore'}
                          </Button>
                        </TableCell>
                      </TableRow>
                    );
                  })}
                </TableBody>
              </Table>
            </CardContent>
          </Card>

          <DiffPanel configId={configId} versions={load.versions} />
        </>
      ) : null}
    </div>
  );
}

/** Format a Unix-seconds timestamp for display. Runs only in the browser, after
 *  the config/version data is fetched client-side - never during prerender. */
function formatWhen(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleString();
}
