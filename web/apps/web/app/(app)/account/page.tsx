import type { Metadata } from 'next';
import {
  Badge,
  Button,
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
  CardFooter,
} from '@amicu/ui';
import Link from 'next/link';
import { TIER_LABELS } from '@amicu/auth';
import { requireSession } from '@/lib/auth/session';
import { LogoutButton } from '@/components/logout-button';
import { TierGate } from '@/components/tier-gate';
import { PageHeader } from '@/components/common/page-header';

export const metadata: Metadata = { title: 'Account' };

export default async function AccountPage() {
  const user = await requireSession();

  return (
    <main className="mx-auto w-full max-w-2xl space-y-6 px-6 py-12">
      <PageHeader title="Account" description="Your profile and plan." />

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Profile</CardTitle>
          <CardDescription>Read from your verified session.</CardDescription>
        </CardHeader>
        <CardContent className="space-y-3 text-sm">
          <Row label="Email">{user.email ?? '—'}</Row>
          <Row label="User ID">
            <span className="font-mono text-xs text-muted-foreground">{user.sub}</span>
          </Row>
          <Row label="Roles">
            {user.roles.length ? (
              <span className="flex flex-wrap gap-1.5">
                {user.roles.map((role) => (
                  <Badge key={role} variant="secondary">
                    {role}
                  </Badge>
                ))}
              </span>
            ) : (
              '—'
            )}
          </Row>
        </CardContent>
        <CardFooter>
          <LogoutButton variant="outline" size="sm" />
        </CardFooter>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle className="text-base">Plan</CardTitle>
          <CardDescription>
            Your current tier. Gating is enforced by the API — this is display only.
          </CardDescription>
        </CardHeader>
        <CardContent className="flex items-center justify-between">
          <div className="flex items-center gap-2">
            <span className="text-sm text-muted-foreground">Current tier</span>
            <Badge>{TIER_LABELS[user.tier]}</Badge>
          </div>
          {user.tier === 'community' ? (
            <Button asChild size="sm" variant="outline">
              <Link href="/billing">Upgrade</Link>
            </Button>
          ) : null}
        </CardContent>
      </Card>

      {/* Example of the presentational tier gate: Pro-only team collaboration. */}
      <TierGate required="pro" feature="Team collaboration">
        <Card>
          <CardHeader>
            <CardTitle className="text-base">Team collaboration</CardTitle>
            <CardDescription>
              Shared state, locking, and roles for your team.
            </CardDescription>
          </CardHeader>
          <CardContent className="text-sm text-muted-foreground">
            Manage members and shared stacks here.
          </CardContent>
        </Card>
      </TierGate>
    </main>
  );
}

function Row({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="flex items-start justify-between gap-4">
      <span className="text-muted-foreground">{label}</span>
      <span className="text-right">{children}</span>
    </div>
  );
}
