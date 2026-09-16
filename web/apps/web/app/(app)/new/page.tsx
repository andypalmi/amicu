import type { Metadata } from 'next';
import Link from 'next/link';
import { Button, Card, CardContent, CardDescription, CardHeader, CardTitle } from '@amicu/ui';
import { requireSession } from '@/lib/auth/session';
import { PageHeader } from '@/components/common/page-header';

export const metadata: Metadata = { title: 'New configuration' };

// Dual-mode entry point (andypalmi/amicu-planning#85): "Help me choose" sends
// the user through the guided wizard at `/new/guided` (preset -> questions ->
// candidates -> review -> confirm); "I know my stack" skips straight to a
// blank-HCL editor at `/new/manual`. Neither path touches the backend
// directly - both eventually go through the same `/api/*` BFF routes the
// classic `/wizard` already uses.
export default async function NewConfigPage() {
  await requireSession();

  return (
    <main className="mx-auto w-full max-w-4xl space-y-8 px-6 py-12">
      <PageHeader
        title="New configuration"
        description="Start from a guided flow that recommends a stack for your workload, or jump straight into writing HCL yourself."
      />

      <div className="grid gap-4 sm:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle>Help me choose</CardTitle>
            <CardDescription>
              Answer a few questions about your workload and pick from a handful of ranked
              candidate stacks.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Button asChild className="w-full">
              <Link href="/new/guided">Start the guided wizard</Link>
            </Button>
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle>I know my stack</CardTitle>
            <CardDescription>
              Skip straight to an empty HCL editor - write, validate, and save your configuration
              yourself.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <Button asChild variant="outline" className="w-full">
              <Link href="/new/manual">Start from a blank configuration</Link>
            </Button>
          </CardContent>
        </Card>
      </div>

      <p className="text-sm text-muted-foreground">
        Prefer the classic flow?{' '}
        <Link href="/wizard" className="underline underline-offset-2">
          Use the preset wizard
        </Link>
        .
      </p>
    </main>
  );
}
