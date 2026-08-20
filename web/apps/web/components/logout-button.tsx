'use client';

import { useFormStatus } from 'react-dom';
import { Button, type ButtonProps } from '@amicu/ui';
import { logoutAction } from '@/app/(auth)/actions';

function SubmitButton(props: ButtonProps) {
  const { pending } = useFormStatus();
  return (
    <Button type="submit" disabled={pending} {...props}>
      {pending ? 'Signing out…' : 'Sign out'}
    </Button>
  );
}

export function LogoutButton(props: ButtonProps) {
  return (
    <form action={logoutAction}>
      <SubmitButton {...props} />
    </form>
  );
}
