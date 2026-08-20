'use client';

import { useEffect, useState } from 'react';
import Link from 'next/link';
import { useSearchParams } from 'next/navigation';
import {
  Badge,
  Button,
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
  CardFooter,
  Alert,
  AlertTitle,
  AlertDescription,
} from '@amicu/ui';
import {
  looksLikeBillingStatus,
  looksLikeRedirect,
  planLabel,
  nextAction,
  type BillingStatus,
} from '@/lib/billing/format';
import { useTier } from '@/components/session-provider';

type Load =
  | { kind: 'loading' }
  | { kind: 'ready'; status: BillingStatus }
  | { kind: 'error'; message: string };

// The billing console. Reads the user's real billing status from the API (is
// Stripe configured, what is the current plan, is there an active subscription)
// and offers exactly the action that status supports — Upgrade (checkout) or
// Manage (portal) — hitting the /api/billing/* BFF routes. When billing is not
// configured server-side it says so honestly rather than showing buttons that
// would 503. No plan or price is displayed that the API did not confirm.
export function BillingConsole() {
  const tokenTier = useTier();
  const [load, setLoad] = useState<Load>({ kind: 'loading' });
  const [pending, setPending] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  // The post-checkout redirect outcome Stripe appends (?checkout=...), derived
  // during render from the URL (no effect, no hydration mismatch).
  const checkoutParam = useSearchParams().get('checkout');
  const checkoutBanner: 'success' | 'cancel' | null =
    checkoutParam === 'success' || checkoutParam === 'cancel' ? checkoutParam : null;

  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const res = await fetch('/api/billing/status');
        const payload: unknown = await res.json().catch(() => null);
        if (cancelled) return;
        if (res.status === 401) {
          setLoad({
            kind: 'error',
            message: 'Your session has expired. Reload the page or sign in again.',
          });
          return;
        }
        if (res.ok && looksLikeBillingStatus(payload)) {
          setLoad({ kind: 'ready', status: payload });
          return;
        }
        setLoad({ kind: 'error', message: 'Could not load your billing status.' });
      } catch {
        if (!cancelled) setLoad({ kind: 'error', message: 'The billing service is unreachable.' });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, []);

  async function startCheckout(plan: string) {
    setPending(true);
    setActionError(null);
    try {
      const res = await fetch('/api/billing/checkout', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ plan }),
      });
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok && looksLikeRedirect(payload)) {
        window.location.href = payload.url;
        return;
      }
      setActionError(errorMessage(res.status, payload));
    } catch {
      setActionError('The billing service is unreachable.');
    } finally {
      setPending(false);
    }
  }

  async function openPortal() {
    setPending(true);
    setActionError(null);
    try {
      const res = await fetch('/api/billing/portal', { method: 'POST' });
      const payload: unknown = await res.json().catch(() => null);
      if (res.ok && looksLikeRedirect(payload)) {
        window.location.href = payload.url;
        return;
      }
      setActionError(errorMessage(res.status, payload));
    } catch {
      setActionError('The billing service is unreachable.');
    } finally {
      setPending(false);
    }
  }

  // The authoritative current plan is the API's; before it loads (or on error)
  // fall back to the tier carried by the verified session token.
  const currentPlan = load.kind === 'ready' ? load.status.plan : (tokenTier ?? 'community');

  return (
    <div className="space-y-6">
      {checkoutBanner === 'success' ? (
        <Alert>
          <AlertTitle>Checkout complete</AlertTitle>
          <AlertDescription>
            Thanks! Your new plan takes effect the next time you sign in or your session refreshes.
          </AlertDescription>
        </Alert>
      ) : null}
      {checkoutBanner === 'cancel' ? (
        <Alert>
          <AlertTitle>Checkout cancelled</AlertTitle>
          <AlertDescription>No change was made to your plan.</AlertDescription>
        </Alert>
      ) : null}

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Current plan</CardTitle>
          <CardDescription>
            Read from your account. A plan change applies the next time you sign in or your session
            refreshes — active sessions keep their current access until then.
          </CardDescription>
        </CardHeader>
        <CardContent className="flex items-center gap-2">
          <span className="text-sm text-muted-foreground">Plan</span>
          <Badge>{planLabel(currentPlan)}</Badge>
        </CardContent>
        <CardFooter className="flex-col items-stretch gap-3">
          {load.kind === 'loading' ? (
            <p className="text-sm text-muted-foreground">Loading billing status…</p>
          ) : null}

          {load.kind === 'error' ? (
            <p className="text-sm text-muted-foreground">{load.message}</p>
          ) : null}

          {load.kind === 'ready' ? <ActionArea
            status={load.status}
            pending={pending}
            onUpgrade={() => startCheckout('pro')}
            onManage={openPortal}
          /> : null}

          {actionError ? (
            <p className="text-sm text-destructive">{actionError}</p>
          ) : null}
        </CardFooter>
      </Card>

      <p className="text-xs text-muted-foreground">
        Compare what each plan includes on the{' '}
        <Link href="/pricing" className="underline underline-offset-4">
          pricing page
        </Link>
        .
      </p>
    </div>
  );
}

function ActionArea({
  status,
  pending,
  onUpgrade,
  onManage,
}: {
  status: BillingStatus;
  pending: boolean;
  onUpgrade: () => void;
  onManage: () => void;
}) {
  const action = nextAction(status);

  if (action === 'unavailable') {
    // Honest: Stripe isn't configured server-side, so we show no buttons that
    // would just 503. Point to pricing where plans are described.
    return (
      <p className="text-sm text-muted-foreground">
        Self-serve billing isn&apos;t available on this instance yet.{' '}
        <Link href="/pricing" className="underline underline-offset-4">
          See plans
        </Link>{' '}
        or contact us to upgrade.
      </p>
    );
  }

  if (action === 'manage') {
    return (
      <Button onClick={onManage} disabled={pending} size="sm" variant="outline">
        {pending ? 'Opening…' : 'Manage billing'}
      </Button>
    );
  }

  return (
    <div className="flex flex-col gap-2">
      <Button onClick={onUpgrade} disabled={pending} size="sm">
        {pending ? 'Redirecting…' : 'Upgrade to Pro'}
      </Button>
      <span className="text-xs text-muted-foreground">
        You&apos;ll be redirected to Stripe to complete checkout securely.
      </span>
    </div>
  );
}

/** Map a non-OK billing response to a friendly message, preferring the API's. */
function errorMessage(status: number, payload: unknown): string {
  const apiError =
    payload && typeof payload === 'object' && typeof (payload as { error?: unknown }).error === 'string'
      ? (payload as { error: string }).error
      : null;
  if (status === 503) return 'Billing is not configured on this instance yet.';
  if (status === 401) return 'Your session has expired. Reload the page or sign in again.';
  if (status === 409) return 'You have no billing account yet — start a checkout first.';
  return apiError ?? `Request failed (${status}).`;
}
