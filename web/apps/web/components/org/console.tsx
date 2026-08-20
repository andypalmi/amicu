'use client';

import { useCallback, useEffect, useState } from 'react';
import {
  Badge,
  Button,
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
  Input,
  Label,
  Separator,
} from '@amicu/ui';
import {
  roleLabel,
  actionLabel,
  permissionLabel,
  assignableRoles,
  canManageMembers,
  canActOnTarget,
  looksLikeMembersResponse,
  looksLikeRolesResponse,
  looksLikeAuditResponse,
  type MembersResponse,
  type MemberView,
  type RoleView,
  type AuditEntryView,
} from '@/lib/org/format';

// The organization console. Reads REAL data from the Management API through the
// /api/org/* BFF: the member list (with the caller's own role), the RBAC catalog,
// and the audit log. It offers exactly the actions the caller's role allows —
// add/remove/change-role — and surfaces the API's 403 (RBAC) / 404 / 409
// responses inline rather than pretending an action succeeded. The API is the
// sole authority; this component never invents membership or an entitlement.
//
// Tier gating is done by the page (via <TierGate required="enterprise">), so this
// component only renders for Enterprise sessions. A 403 seen HERE is therefore an
// org-role (RBAC) denial, shown as an inline error — not a tier upsell.

type MembersLoad =
  | { kind: 'loading' }
  | { kind: 'ready'; data: MembersResponse }
  | { kind: 'error'; message: string };

export function OrgConsole() {
  const [members, setMembers] = useState<MembersLoad>({ kind: 'loading' });
  const [roles, setRoles] = useState<RoleView[] | null>(null);
  const [audit, setAudit] = useState<AuditEntryView[] | null>(null);
  const [auditNote, setAuditNote] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [pending, setPending] = useState(false);

  const loadMembers = useCallback(async () => {
    try {
      const res = await fetch('/api/org/members');
      const payload: unknown = await res.json().catch(() => null);
      if (res.status === 401) {
        setMembers({
          kind: 'error',
          message: 'Your session has expired. Reload the page or sign in again.',
        });
        return;
      }
      if (res.ok && looksLikeMembersResponse(payload)) {
        setMembers({ kind: 'ready', data: payload });
        return;
      }
      setMembers({ kind: 'error', message: apiError(payload) ?? 'Could not load organization members.' });
    } catch {
      setMembers({ kind: 'error', message: 'The organization service is unreachable.' });
    }
  }, []);

  const loadRoles = useCallback(async () => {
    try {
      const res = await fetch('/api/org/roles');
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok && looksLikeRolesResponse(payload)) setRoles(payload.roles);
    } catch {
      /* roles catalog is non-critical; leave it unshown on failure */
    }
  }, []);

  const loadAudit = useCallback(async () => {
    try {
      const res = await fetch('/api/org/audit');
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok && looksLikeAuditResponse(payload)) {
        setAudit(payload.entries);
        setAuditNote(null);
        return;
      }
      if (res.status === 403) {
        setAudit(null);
        setAuditNote('Your role cannot view the audit log (Owner or Admin only).');
        return;
      }
      setAuditNote('Could not load the audit log.');
    } catch {
      setAuditNote('The organization service is unreachable.');
    }
  }, []);

  useEffect(() => {
    // Load in an async IIFE so no setState runs synchronously in the effect body
    // (each loader only sets state after its `await fetch`).
    void (async () => {
      await Promise.all([loadMembers(), loadRoles(), loadAudit()]);
    })();
  }, [loadMembers, loadRoles, loadAudit]);

  // Re-read the member list and audit log after a successful mutation.
  const refresh = useCallback(async () => {
    await Promise.all([loadMembers(), loadAudit()]);
  }, [loadMembers, loadAudit]);

  async function addMember(userId: string, role: string) {
    setPending(true);
    setActionError(null);
    setNotice(null);
    try {
      const res = await fetch('/api/org/members', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_id: userId, role }),
      });
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok) {
        setNotice(`Added ${userId} as ${roleLabel(role)}.`);
        await refresh();
        return;
      }
      setActionError(errorMessage(res.status, payload));
    } catch {
      setActionError('The organization service is unreachable.');
    } finally {
      setPending(false);
    }
  }

  async function changeRole(userId: string, role: string) {
    setPending(true);
    setActionError(null);
    setNotice(null);
    try {
      const res = await fetch(`/api/org/members/${encodeURIComponent(userId)}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ role }),
      });
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok) {
        setNotice(`Changed ${userId} to ${roleLabel(role)}.`);
        await refresh();
        return;
      }
      setActionError(errorMessage(res.status, payload));
      // Re-sync so the select snaps back to the real (unchanged) role.
      await loadMembers();
    } catch {
      setActionError('The organization service is unreachable.');
    } finally {
      setPending(false);
    }
  }

  async function removeMember(userId: string) {
    setPending(true);
    setActionError(null);
    setNotice(null);
    try {
      const res = await fetch(`/api/org/members/${encodeURIComponent(userId)}`, {
        method: 'DELETE',
      });
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok) {
        setNotice(`Removed ${userId}.`);
        await refresh();
        return;
      }
      setActionError(errorMessage(res.status, payload));
    } catch {
      setActionError('The organization service is unreachable.');
    } finally {
      setPending(false);
    }
  }

  return (
    <div className="space-y-6">
      {notice ? (
        <p className="rounded-lg border border-border bg-muted/40 p-3 text-sm">{notice}</p>
      ) : null}
      {actionError ? <p className="text-sm text-destructive">{actionError}</p> : null}

      <MembersCard
        load={members}
        pending={pending}
        onAdd={addMember}
        onChangeRole={changeRole}
        onRemove={removeMember}
      />

      <RolesCard roles={roles} />

      <AuditCard entries={audit} note={auditNote} />
    </div>
  );
}

function MembersCard({
  load,
  pending,
  onAdd,
  onChangeRole,
  onRemove,
}: {
  load: MembersLoad;
  pending: boolean;
  onAdd: (userId: string, role: string) => void;
  onChangeRole: (userId: string, role: string) => void;
  onRemove: (userId: string) => void;
}) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Members</CardTitle>
        <CardDescription>
          Everyone in your organization and their role. Role changes and removals are enforced by
          the API according to your own role.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-6">
        {load.kind === 'loading' ? (
          <p className="text-sm text-muted-foreground">Loading members…</p>
        ) : null}

        {load.kind === 'error' ? (
          <p className="text-sm text-muted-foreground">{load.message}</p>
        ) : null}

        {load.kind === 'ready' ? (
          <MembersTable
            data={load.data}
            pending={pending}
            onChangeRole={onChangeRole}
            onRemove={onRemove}
          />
        ) : null}

        {load.kind === 'ready' && canManageMembers(load.data.caller_role) ? (
          <>
            <Separator />
            <AddMemberForm callerRole={load.data.caller_role} pending={pending} onAdd={onAdd} />
          </>
        ) : null}
      </CardContent>
    </Card>
  );
}

function MembersTable({
  data,
  pending,
  onChangeRole,
  onRemove,
}: {
  data: MembersResponse;
  pending: boolean;
  onChangeRole: (userId: string, role: string) => void;
  onRemove: (userId: string) => void;
}) {
  const caller = data.caller_role;
  const options = assignableRoles(caller);

  return (
    <div className="overflow-x-auto">
      <table className="w-full text-sm">
        <thead>
          <tr className="border-b border-border text-left text-xs uppercase tracking-wide text-muted-foreground">
            <th className="py-2 pr-4 font-medium">User</th>
            <th className="py-2 pr-4 font-medium">Role</th>
            <th className="py-2 pr-4 font-medium">Actions</th>
          </tr>
        </thead>
        <tbody>
          {data.members.map((m: MemberView) => {
            const actable = canManageMembers(caller) && canActOnTarget(caller, m.role) && options.length > 0;
            return (
              <tr key={m.user_id} className="border-b border-border/60">
                <td className="py-2 pr-4 font-mono text-xs">{m.user_id}</td>
                <td className="py-2 pr-4">
                  {actable ? (
                    <select
                      aria-label={`Role for ${m.user_id}`}
                      value={m.role}
                      disabled={pending}
                      onChange={(e) => {
                        if (e.target.value !== m.role) onChangeRole(m.user_id, e.target.value);
                      }}
                      className="rounded-md border border-border bg-card px-2 py-1 text-sm text-card-foreground focus:outline-none focus:ring-2 focus:ring-ring"
                    >
                      {/* Ensure the current role is selectable even if not in the
                          caller's normal assignable set. */}
                      {(options.includes(m.role as never) ? options : [m.role, ...options]).map(
                        (r) => (
                          <option key={r} value={r}>
                            {roleLabel(r)}
                          </option>
                        ),
                      )}
                    </select>
                  ) : (
                    <Badge variant="outline">{roleLabel(m.role)}</Badge>
                  )}
                </td>
                <td className="py-2 pr-4">
                  {actable ? (
                    <Button
                      size="sm"
                      variant="outline"
                      disabled={pending}
                      onClick={() => onRemove(m.user_id)}
                    >
                      Remove
                    </Button>
                  ) : (
                    <span className="text-xs text-muted-foreground">—</span>
                  )}
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

function AddMemberForm({
  callerRole,
  pending,
  onAdd,
}: {
  callerRole: string;
  pending: boolean;
  onAdd: (userId: string, role: string) => void;
}) {
  const options = assignableRoles(callerRole);
  const [userId, setUserId] = useState('');
  const [role, setRole] = useState<string>(options[options.length - 1] ?? 'viewer');

  return (
    <form
      className="flex flex-wrap items-end gap-3"
      onSubmit={(e) => {
        e.preventDefault();
        if (userId.trim()) onAdd(userId.trim(), role);
      }}
    >
      <div className="space-y-1">
        <Label htmlFor="new-user-id">User id</Label>
        <Input
          id="new-user-id"
          value={userId}
          onChange={(e) => setUserId(e.target.value)}
          placeholder="usr_…"
          className="font-mono"
          autoComplete="off"
        />
      </div>
      <div className="space-y-1">
        <Label htmlFor="new-role">Role</Label>
        <select
          id="new-role"
          value={role}
          onChange={(e) => setRole(e.target.value)}
          className="block rounded-md border border-border bg-card px-2 py-2 text-sm text-card-foreground focus:outline-none focus:ring-2 focus:ring-ring"
        >
          {options.map((r) => (
            <option key={r} value={r}>
              {roleLabel(r)}
            </option>
          ))}
        </select>
      </div>
      <Button type="submit" size="sm" disabled={pending || userId.trim() === ''}>
        {pending ? 'Working…' : 'Add member'}
      </Button>
    </form>
  );
}

function RolesCard({ roles }: { roles: RoleView[] | null }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Roles &amp; permissions</CardTitle>
        <CardDescription>
          The organization&apos;s RBAC model, served by the API. These are the permissions each
          role holds — the API checks them on every request.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {roles === null ? (
          <p className="text-sm text-muted-foreground">Loading roles…</p>
        ) : (
          <ul className="space-y-4">
            {roles.map((r) => (
              <li key={r.role} className="space-y-2">
                <div className="flex items-center gap-2">
                  <Badge>{roleLabel(r.role)}</Badge>
                  <span className="text-xs text-muted-foreground">{r.description}</span>
                </div>
                <div className="flex flex-wrap gap-1.5">
                  {r.permissions.map((p) => (
                    <Badge key={p} variant="outline" className="font-normal">
                      {permissionLabel(p)}
                    </Badge>
                  ))}
                </div>
              </li>
            ))}
          </ul>
        )}
      </CardContent>
    </Card>
  );
}

function AuditCard({ entries, note }: { entries: AuditEntryView[] | null; note: string | null }) {
  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">Audit log</CardTitle>
        <CardDescription>
          Every privileged action, most-recent first. Recorded server-side and append-only.
        </CardDescription>
      </CardHeader>
      <CardContent>
        {note ? <p className="text-sm text-muted-foreground">{note}</p> : null}
        {entries !== null ? (
          entries.length === 0 ? (
            <p className="text-sm text-muted-foreground">No actions recorded yet.</p>
          ) : (
            <ul className="space-y-2">
              {entries.map((e) => (
                <li
                  key={e.id}
                  className="flex flex-wrap items-baseline gap-x-2 gap-y-1 border-b border-border/60 pb-2 text-sm"
                >
                  <span className="font-medium">{actionLabel(e.action)}</span>
                  {e.target_id ? (
                    <span className="font-mono text-xs text-muted-foreground">{e.target_id}</span>
                  ) : null}
                  {e.detail ? <span className="text-xs text-muted-foreground">({e.detail})</span> : null}
                  <span className="ml-auto text-xs text-muted-foreground">
                    by <span className="font-mono">{e.actor_id}</span> · {formatWhen(e.timestamp)}
                  </span>
                </li>
              ))}
            </ul>
          )
        ) : null}
      </CardContent>
    </Card>
  );
}

/** Format a Unix-seconds timestamp for display. Runs only in the browser, after
 *  the audit log is fetched client-side — never during prerender. */
function formatWhen(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleString();
}

/** The API's own error string, if the body carries one. */
function apiError(payload: unknown): string | null {
  return payload && typeof payload === 'object' && typeof (payload as { error?: unknown }).error === 'string'
    ? (payload as { error: string }).error
    : null;
}

/** Map a non-OK org response to a friendly message, preferring the API's own. */
function errorMessage(status: number, payload: unknown): string {
  const err = apiError(payload);
  if (status === 401) return 'Your session has expired. Reload the page or sign in again.';
  if (status === 403) return err ?? 'Your role does not permit that action.';
  if (status === 404) return err ?? 'That member no longer exists.';
  if (status === 409) return err ?? 'That action conflicts with the current membership.';
  if (status === 502) return 'The organization service is unreachable.';
  return err ?? `Request failed (${status}).`;
}
