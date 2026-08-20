import type { Metadata } from 'next';
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
} from '@amicu/ui';
import { SignupForm } from '@/components/auth/signup-form';

export const metadata: Metadata = { title: 'Create account' };

export default async function SignupPage({ searchParams }: PageProps<'/signup'>) {
  const { next } = await searchParams;
  const nextPath = typeof next === 'string' ? next : undefined;

  return (
    <Card>
      <CardHeader>
        <CardTitle>Create your account</CardTitle>
        <CardDescription>
          Start on Community — no card required. Upgrade any time.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <SignupForm next={nextPath} />
      </CardContent>
    </Card>
  );
}
