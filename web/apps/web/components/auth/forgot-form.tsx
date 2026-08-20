'use client';

import Link from 'next/link';
import { useActionState } from 'react';
import { Alert, AlertDescription } from '@amicu/ui';
import { forgotAction, type AuthFormState } from '@/app/(auth)/actions';
import { FormField } from './form-field';
import { SubmitButton } from './submit-button';

const initial: AuthFormState = {};

export function ForgotForm() {
  const [state, action] = useActionState(forgotAction, initial);

  return (
    <form action={action} className="space-y-4" noValidate>
      {state.notice ? (
        <Alert variant="success">
          <AlertDescription>{state.notice}</AlertDescription>
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

      <SubmitButton pendingLabel="Sending…">Send reset link</SubmitButton>

      <p className="text-center text-sm text-muted-foreground">
        Remembered it?{' '}
        <Link href="/login" className="font-medium text-foreground hover:underline">
          Back to sign in
        </Link>
      </p>
    </form>
  );
}
