import Link from 'next/link';
import { SIGNIN_ENABLED } from '@/lib/features';
import { appHomeHref, siteHref } from '@/lib/shared';
import { Compass, Home } from 'lucide-react';
import {
  Button,
  Card,
  CardContent,
  CardDescription,
  CardFooter,
  CardHeader,
  CardTitle,
} from '@amicu/ui';

export const metadata = { title: 'Page not found' };

// Branded root 404. Rendered by Next.js for unmatched routes and for explicit
// `notFound()` calls that aren't caught by a nearer segment boundary.
export default function NotFound() {
  return (
    <main className="mx-auto flex w-full max-w-2xl flex-1 flex-col justify-center px-6 py-16">
      <Card>
        <CardHeader>
          <p className="text-sm font-medium text-muted-foreground">
            404
          </p>
          <CardTitle className="text-2xl">Page not found</CardTitle>
          <CardDescription>
            We couldn&apos;t find the page you were looking for. It may have moved, or the
            link may be out of date.
          </CardDescription>
        </CardHeader>
        <CardContent className="text-sm text-muted-foreground">
          Check the address, or jump back to a known-good starting point below.
        </CardContent>
        <CardFooter className="gap-3">
          <Button asChild>
            <Link href={siteHref('/')}>
              <Home /> Go home
            </Link>
          </Button>
          {SIGNIN_ENABLED ? (
            <Button asChild variant="outline">
              <Link href={appHomeHref()}>
                <Compass /> Open the console
              </Link>
            </Button>
          ) : (
            <Button asChild variant="outline">
              <Link href="/docs">
                <Compass /> Read the docs
              </Link>
            </Button>
          )}
        </CardFooter>
      </Card>
    </main>
  );
}
