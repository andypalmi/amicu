'use client';

import { useEffect } from 'react';
import Link from 'next/link';
import { AlertTriangle, RotateCcw } from 'lucide-react';
import {
  Alert,
  AlertDescription,
  AlertTitle,
  Button,
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from '@amicu/ui';
import { reportClientError } from '@/lib/observability/report-error';

// Route-segment error boundary. Catches render/runtime throws from any page in
// the app tree (below the root layout) and offers a `reset()` recovery path.
export default function Error({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  useEffect(() => {
    reportClientError(error, 'route-error');
  }, [error]);

  return (
    <main className="mx-auto flex w-full max-w-2xl flex-1 flex-col justify-center px-6 py-16">
      <Card>
        <CardHeader>
          <AlertTriangle className="size-6 text-destructive" aria-hidden />
          <CardTitle className="text-2xl">Something went wrong</CardTitle>
          <CardDescription>
            An unexpected error interrupted this page. You can try again - if it keeps
            happening, head back to the console or reach out.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <Alert variant="destructive">
            <AlertTitle>Error details</AlertTitle>
            <AlertDescription>
              <p>{error.message || 'An unknown error occurred.'}</p>
              {error.digest ? (
                <p className="mt-1 font-mono text-xs text-muted-foreground">
                  Reference: {error.digest}
                </p>
              ) : null}
            </AlertDescription>
          </Alert>
        </CardContent>
        <CardFooter className="gap-3">
          <Button onClick={reset}>
            <RotateCcw /> Try again
          </Button>
          <Button asChild variant="outline">
            <Link href="/app">Back to console</Link>
          </Button>
        </CardFooter>
      </Card>
    </main>
  );
}
