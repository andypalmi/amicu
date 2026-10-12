'use client';

import { useActionState } from 'react';
import { Alert, AlertDescription } from '@amicu/ui';
import { resendVerificationAction, type AuthFormState } from '@/app/(auth)/actions';
import { TurnstileWidget } from './turnstile-widget';
import { FormField } from './form-field';
import { SubmitButton } from './submit-button';

const initial: AuthFormState = {};

// Resend the "confirm your email" link from the /verify page's invalid/expired
// states. The verification token never round-trips to the browser and carries no
// email, so the user re-enters their address here. The outcome is intentionally
// opaque (anti-enumeration): the confirmation reads the same whether or not an
// unverified account exists for that address.
export function ResendVerificationForm({ siteKey }: { siteKey?: string | null }) {
  const [state, action] = useActionState(resendVerificationAction, initial);

  return (
    <form action={action} className="space-y-4" noValidate>
      {state.notice ? (
        <Alert variant="success">
          <AlertDescription>{state.notice}</AlertDescription>
        </Alert>
      ) : null}

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

      <TurnstileWidget siteKey={siteKey} action="resend-verification" resetKey={state} />

      <SubmitButton pendingLabel="Sending…">Resend verification email</SubmitButton>
    </form>
  );
}
