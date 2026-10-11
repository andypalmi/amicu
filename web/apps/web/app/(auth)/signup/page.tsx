import type { Metadata } from 'next';
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
} from '@amicu/ui';
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
        <SignupForm />
      </CardContent>
    </Card>
  );
}
