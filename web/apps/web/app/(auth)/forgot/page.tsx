import type { Metadata } from 'next';
import {
  Card,
  CardHeader,
  CardTitle,
  CardDescription,
  CardContent,
} from '@amicu/ui';
import { ForgotForm } from '@/components/auth/forgot-form';

export const metadata: Metadata = { title: 'Reset password' };

export default function ForgotPage() {
  return (
    <Card>
      <CardHeader>
        <CardTitle>Reset your password</CardTitle>
        <CardDescription>
          Enter your email and we&apos;ll send a reset link.
        </CardDescription>
      </CardHeader>
      <CardContent>
        <ForgotForm />
      </CardContent>
    </Card>
  );
}
