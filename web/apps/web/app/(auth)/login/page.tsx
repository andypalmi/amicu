import type { Metadata } from 'next';
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
} from '@amicu/ui';
import { LoginForm } from '@/components/auth/login-form';

export const metadata: Metadata = { title: 'Sign in' };

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
        <LoginForm next={nextPath} />
      </CardContent>
    </Card>
  );
}
