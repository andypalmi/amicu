import type { Metadata } from 'next';
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
} from '@amicu/ui';
import { turnstileSiteKey } from '@/lib/security/turnstile-config';
import Link from 'next/link';
import { SIGNUPS_ENABLED } from '@/lib/auth/config';
import { SignupForm } from '@/components/auth/signup-form';

export const metadata: Metadata = {
  title: 'Create account',
  description:
    'Create a free amicu account and start designing LLM serving stacks.',
};

export default function SignupPage() {
  if (!SIGNUPS_ENABLED) {
    return (
      <Card>
        <CardHeader>
          <CardTitle>Sign-ups are currently closed</CardTitle>
          <CardDescription>
            We are not accepting new accounts right now. Already have one?{' '}
            <Link href="/login" className="font-medium text-foreground hover:underline">
              Sign in
            </Link>
            .
          </CardDescription>
        </CardHeader>
      </Card>
    );
  }
  return (
    <Card>
      <CardHeader>
        <CardTitle>Create your account</CardTitle>
        <CardDescription>
          Start on Community - no card required. Upgrade any time.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <SignupForm siteKey={turnstileSiteKey()} />
      </CardContent>
    </Card>
  );
}
