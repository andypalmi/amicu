import type { Metadata } from 'next';
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
} from '@amicu/ui';
import { SIGNUPS_ENABLED } from '@/lib/auth/config';
import { LoginForm } from '@/components/auth/login-form';

export const metadata: Metadata = {
  title: 'Sign in',
  description:
    'Sign in to your Amicu account to design, deploy, and operate LLM serving stacks.',
};

export default async function LoginPage({
  searchParams,
}: PageProps<'/login'>) {
  const { next } = await searchParams;
  const nextPath = typeof next === 'string' ? next : undefined;

  return (
    <Card>
      <CardHeader>
        <CardTitle>Welcome back</CardTitle>
        <CardDescription>Sign in to your Amicu account.</CardDescription>
      </CardHeader>
      <CardContent>
        <LoginForm next={nextPath} signupsEnabled={SIGNUPS_ENABLED} />
      </CardContent>
    </Card>
  );
}
