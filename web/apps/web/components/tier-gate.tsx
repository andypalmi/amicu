'use client';

import Link from 'next/link';
import type { ReactNode } from 'react';
import { Button } from '@amicu/ui';
import { Card, CardHeader, CardTitle, CardDescription, CardContent } from '@amicu/ui';
import { TIER_LABELS, tierSatisfies, type Tier } from '@amicu/auth';
import { useTier } from './session-provider';

// ============================================================================
// PRESENTATIONAL ONLY — NOT A SECURITY BOUNDARY.
//
// <TierGate> decides whether to SHOW a feature or render an upsell. It is NEVER
// an enforcement mechanism. The tier it reads comes from a claim the frontend
// does not control, and the whole component tree is shipped to the browser. The
// Rust Management API re-checks the caller's tier on every mutation and rejects
// unauthorized actions regardless of what is rendered here. Do not gate anything
// that must be secure behind this — gate it on the API.
// ============================================================================

export interface TierGateProps {
  /**
   * Required tier. A single tier is an "at least this tier" ladder check; an array
   * is an explicit allow-list (for non-linear gates). Mirrors the API tier enum.
   */
  required: Tier | readonly Tier[];
  children: ReactNode;
  /** Custom locked-state UI. Defaults to <TierUpsell>. */
  fallback?: ReactNode;
  /** Human name of the gated feature, shown in the default upsell. */
  feature?: string;
  /**
   * Tier override, mainly for previews/tests. When omitted the current session
   * tier is used. Unauthenticated users are treated as below every gate.
   */
  tier?: Tier | null;
}

export function TierGate({ required, children, fallback, feature, tier }: TierGateProps) {
  // Always call the hook (Rules of Hooks); the explicit `tier` prop overrides it.
  const sessionTier = useTier();
  const current = tier !== undefined ? tier : sessionTier;
  const unlocked = current !== null && tierSatisfies(current, required);

  if (unlocked) return <>{children}</>;
  return <>{fallback ?? <TierUpsell required={required} feature={feature} />}</>;
}

function requiredLabel(required: Tier | readonly Tier[]): string {
  if (Array.isArray(required)) {
    return (required as readonly Tier[]).map((t) => TIER_LABELS[t]).join(' or ');
  }
  return TIER_LABELS[required as Tier];
}

/** Default locked-state card prompting an upgrade. */
export function TierUpsell({
  required,
  feature,
}: {
  required: Tier | readonly Tier[];
  feature?: string;
}) {
  const label = requiredLabel(required);
  return (
    <Card className="border-dashed">
      <CardHeader>
        <CardTitle className="text-base">
          {feature ? `${feature} is a ${label} feature` : `Available on ${label}`}
        </CardTitle>
        <CardDescription>
          Upgrade your plan to unlock {feature ? feature.toLowerCase() : 'this feature'}.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <Button asChild size="sm">
          <Link href="/pricing">View plans</Link>
        </Button>
      </CardContent>
    </Card>
  );
}
