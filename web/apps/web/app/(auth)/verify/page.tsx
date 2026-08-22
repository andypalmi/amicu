import type { Metadata } from 'next';
import Link from 'next/link';
import {
  Alert,
  AlertDescription,
  Button,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@amicu/ui';
import { verifyAction } from '@/app/(auth)/actions';

export const metadata: Metadata = { title: 'Verify email' };

// Landing page for the verification link mailed at sign-up. The email points at
// `{web_base}/verify?token={token}` (the contract with the SMTP sender), so this
// route reads `?token=` and confirms it against the Management API on load.
//
// This is a Server Component: it awaits `searchParams`, calls the `verifyAction`
// server action (which reaches `POST /verify` through the same BFF auth client the
// other auth flows use), and renders a static result. No token round-trips to the
// browser. An absent or garbage token is handled without a call / without crashing.
export default async function VerifyPage({
  searchParams,
}: PageProps<'/verify'>) {
  const { token } = await searchParams;
  const rawToken = typeof token === 'string' ? token : '';

  // Missing token: the link was mangled or opened without the query. Don't call
  // the API; guide the user rather than showing a false "invalid" verdict.
  if (!rawToken) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Verification link incomplete</CardTitle>
          <CardDescription>
            This link is missing its verification token. Open the most recent
            confirmation email and use the button or full link it contains.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <Button asChild variant="outline" className="w-full">
            <Link href="/login">Go to sign in</Link>
          </Button>
          <p className="text-center text-sm text-muted-foreground">
            Still stuck? Try signing up again or contact support.
          </p>
        </CardContent>
      </Card>
    );
  }

  const status = await verifyAction(rawToken);

  if (status === 'success') {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Email verified</CardTitle>
          <CardDescription>
            Your email address is confirmed. You can sign in now.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <Alert variant="success">
            <AlertDescription>
              Thanks for confirming your account.
            </AlertDescription>
          </Alert>
          <Button asChild className="w-full">
            <Link href="/login">Go to sign in</Link>
          </Button>
        </CardContent>
      </Card>
    );
  }

  if (status === 'error') {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Could not verify right now</CardTitle>
          <CardDescription>
            Something went wrong confirming your email. This is usually
            temporary.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <Button asChild className="w-full">
            <Link href={`/verify?token=${encodeURIComponent(rawToken)}`}>
              Try again
            </Link>
          </Button>
          <p className="text-center text-sm text-muted-foreground">
            If it keeps failing, contact support.
          </p>
        </CardContent>
      </Card>
    );
  }

  // status === 'invalid': the token was unknown, expired, or already used. The
  // backend returns the same generic result for all three (anti-enumeration), so
  // we cannot say which, and neither should the copy.
  return (
    <Card>
      <CardHeader>
        <CardTitle>Link invalid or expired</CardTitle>
        <CardDescription>
          This verification link is invalid or has expired.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <Button asChild variant="outline" className="w-full">
          <Link href="/login">Go to sign in</Link>
        </Button>
        <p className="text-center text-sm text-muted-foreground">
          Already confirmed? Just sign in. Otherwise sign up again or contact
          support.
        </p>
      </CardContent>
    </Card>
  );
}
