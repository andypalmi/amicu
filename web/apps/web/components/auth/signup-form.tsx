'use client';

import Link from 'next/link';
import { useActionState } from 'react';
import { Alert, AlertDescription } from '@amicu/ui';
import { signupAction, type AuthFormState } from '@/app/(auth)/actions';
import { FormField } from './form-field';
import { SubmitButton } from './submit-button';

const initial: AuthFormState = {};

export function SignupForm({ next }: { next?: string }) {
  const [state, action] = useActionState(signupAction, initial);

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
