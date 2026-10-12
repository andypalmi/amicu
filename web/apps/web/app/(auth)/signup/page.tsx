import type { Metadata } from 'next';
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
} from '@amicu/ui';
import { turnstileSiteKey } from '@/lib/security/turnstile-config';
import { legalPages } from '@/lib/legal/company';
import { SignupForm } from '@/components/auth/signup-form';

export const metadata: Metadata = {
  title: 'Create account',
  description:
    'Create a free Amicu account and start designing LLM serving stacks.',
};

export default function SignupPage() {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Create your account</CardTitle>
        <CardDescription>
          Start on Community - no card required. Upgrade any time.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <SignupForm
          siteKey={turnstileSiteKey()}
          legal={{ terms: legalPages.terms, privacy: legalPages.privacy }}
        />
      </CardContent>
    </Card>
  );
}
