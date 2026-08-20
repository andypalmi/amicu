'use client';

import Link from 'next/link';
import { useActionState } from 'react';
import { Alert, AlertDescription, Button } from '@amicu/ui';
import { signupAction, type AuthFormState } from '@/app/(auth)/actions';
import { FormField } from './form-field';
import { SubmitButton } from './submit-button';

const initial: AuthFormState = {};

export function SignupForm() {
  const [state, action] = useActionState(signupAction, initial);

  // Signup returns a 202 acknowledgement (no auto-login). On success we show the
  // generic confirmation and route the user to sign in once they've confirmed.
  if (state.notice) {
    return (
      <div className="space-y-4">
        <Alert variant="success">
          <AlertDescription>{state.notice}</AlertDescription>
        </Alert>
        <Button asChild className="w-full">
          <Link href="/login">Go to sign in</Link>
        </Button>
      </div>
    );
  }

  return (
    <form action={action} className="space-y-4" noValidate>
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
        autoComplete="new-password"
        required
        error={state.fieldErrors?.password}
      />

      <SubmitButton pendingLabel="Creating account…">Create account</SubmitButton>

      <p className="text-center text-sm text-muted-foreground">
        Already have an account?{' '}
        <Link href="/login" className="font-medium text-foreground hover:underline">
          Sign in
        </Link>
      </p>
    </form>
  );
}
