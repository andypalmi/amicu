import Link from 'next/link';
import { Alert, AlertTitle, AlertDescription, Badge, Button } from '@amicu/ui';

// Shared, PERSISTENT in-context error surface for the authenticated consoles
// (engine / recommend / monitor). Unlike a toast, this stays on screen until the
// underlying request succeeds — the user is meant to read the message and retry
// in place. Built on the `@amicu/ui` `<Alert>` (destructive variant) so all
// three consoles render identical markup and pull from the shared design tokens.
//
// For ephemeral, non-blocking feedback ("Saved", "Copied") use `toast` from
// `lib/notify/toast.ts` instead.
export interface ErrorAlertProps {
  /** The error message to display. */
  message: string;
  /** When true, render a call-to-action linking to pricing (tier-denied errors). */
  upgrade?: boolean;
  className?: string;
}

export function ErrorAlert({ message, upgrade, className }: ErrorAlertProps) {
  return (
    <Alert
      variant="destructive"
      className={
        'space-y-3 border-destructive/40 bg-destructive/10' +
        (className ? ` ${className}` : '')
      }
    >
      <AlertTitle className="mb-0 flex items-center gap-2">
        <Badge variant="destructive">Error</Badge>
      </AlertTitle>
      <AlertDescription>
        <p>{message}</p>
      </AlertDescription>
      {upgrade ? (
        <Button asChild size="sm" variant="outline">
          <Link href="/pricing">See plans</Link>
        </Button>
      ) : null}
    </Alert>
  );
}
