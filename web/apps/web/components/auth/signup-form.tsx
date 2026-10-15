'use client';

import Link from 'next/link';
import { useActionState, useState } from 'react';
import { Alert, AlertDescription, Button } from '@amicu/ui';
import { signupAction, type AuthFormState } from '@/app/(auth)/actions';
import { ACCEPT_TERMS_ERROR, ACCEPT_TERMS_FIELD, termsAccepted } from '@/lib/auth/consent';
import { siteHref } from '@/lib/shared';
import { TurnstileWidget } from './turnstile-widget';
import { FormField } from './form-field';
import { SubmitButton } from './submit-button';

const initial: AuthFormState = {};

export function SignupForm({ siteKey }: { siteKey?: string | null }) {
  const [state, action] = useActionState(signupAction, initial);
  const [consentError, setConsentError] = useState<string | null>(null);
  const termsError = consentError ?? state.fieldErrors?.[ACCEPT_TERMS_FIELD];

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
    <form
      action={action}
      className="space-y-4"
      noValidate
      onSubmit={(e) => {
        // Client-side check for fast feedback; signupAction re-checks on the server.
        const ok = termsAccepted(new FormData(e.currentTarget).get(ACCEPT_TERMS_FIELD));
        setConsentError(ok ? null : ACCEPT_TERMS_ERROR);
        if (!ok) e.preventDefault();
      }}
    >
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

      <div className="space-y-1.5">
        <div className="flex items-start gap-2">
          <input
            id="accept-terms"
            type="checkbox"
            name={ACCEPT_TERMS_FIELD}
            required
            aria-invalid={termsError ? true : undefined}
            aria-describedby={termsError ? 'accept-terms-error' : undefined}
            className="mt-0.5 h-4 w-4 shrink-0"
          />
          <label htmlFor="accept-terms" className="text-sm leading-snug">
            I am 18 or older and agree to the{' '}
            <Link href={siteHref('/terms')} className="underline hover:text-foreground">
              Terms
            </Link>{' '}
            and{' '}
            <Link href={siteHref('/privacy')} className="underline hover:text-foreground">
              Privacy Policy
            </Link>
            .
          </label>
        </div>
        {termsError ? (
          <p id="accept-terms-error" className="text-xs text-destructive">
            {termsError}
          </p>
        ) : null}
      </div>

      <TurnstileWidget siteKey={siteKey} action="signup" resetKey={state} />

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
