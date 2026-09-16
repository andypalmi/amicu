'use client';

import { useState } from 'react';
import Link from 'next/link';
import { Button, Card, CardContent, CardDescription, CardHeader, CardTitle, Input, Label } from '@amicu/ui';
import { ErrorAlert } from '@/components/common/error-alert';
import { errorMessage, looksLikeConfigDetail, looksLikeVersionMeta } from '@/lib/configs/format';
import { toast } from '@/lib/notify/toast';

type SaveMode = 'existing' | 'new';

// "Save this configuration" form. Persists the CURRENT HCL text (the caller's
// `hcl` prop) through the /api/configs/* BFF: either as a new version of an
// existing config (POST .../versions) or as a brand new config (POST
// /api/configs). When loaded from an existing config (`sourceConfigId`), "new
// version" is the default; either way, a successful save re-targets this form
// at the resulting config, so a second Save appends another version instead of
// failing on a duplicate name.
//
// Extracted from `components/wizard/hcl-result.tsx` (andypalmi/amicu-planning#85)
// so the guided wizard's confirm/create step can reuse the exact same
// save-a-config behavior as the classic `/wizard` flow - POSTing here as a
// brand-new named config IS the guided wizard's confirm/create hand-off, there
// is no separate backend call for it.
export function SaveConfigForm({
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
