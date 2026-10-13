'use client';

import Link from 'next/link';
import { useActionState } from 'react';
import { Alert, AlertDescription } from '@amicu/ui';
import { loginAction, type AuthFormState } from '@/app/(auth)/actions';
import { FormField } from './form-field';
import { SubmitButton } from './submit-button';

const initial: AuthFormState = {};

export function LoginForm({
  next,
  signupsEnabled = false,
}: {
  next?: string;
  signupsEnabled?: boolean;
}) {
  const [state, action] = useActionState(loginAction, initial);

  return (
    <form action={action} className="space-y-4" noValidate>
      {next ? <input type="hidden" name="next" value={next} /> : null}

      {state.error ? (
        <Alert variant="destructive">
          <AlertDescription>{state.error}</AlertDescription>
        </Alert>
      ) : null}

      <FormField
        label="Email"
        name="email"
        type="email"
        autoComplete="email"
        required
        error={state.fieldErrors?.email}
      />
      <FormField
        label="Password"
        name="password"
        type="password"
        autoComplete="current-password"
        required
        error={state.fieldErrors?.password}
      />

      <div className="text-right text-sm">
        <Link href="/forgot" className="text-muted-foreground hover:text-foreground">
          Forgot password?
        </Link>
      </div>

      <SubmitButton pendingLabel="Signing in…">Sign in</SubmitButton>

      {signupsEnabled ? (
        <p className="text-center text-sm text-muted-foreground">
          New to amicu?{' '}
          <Link href="/signup" className="font-medium text-foreground hover:underline">
            Create an account
          </Link>
        </p>
      ) : (
        <p className="text-center text-sm text-muted-foreground">
          Sign-ups are closed. Using the CLI? You don&apos;t need an account.
        </p>
      )}
    </form>
  );
}
