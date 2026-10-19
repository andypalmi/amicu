import type { ReactNode } from 'react';
import { RUNPOD_SIGNUP_URL } from '@/app/(marketing)/_lib/content';

export function RunPodSignupLink({
  children = 'Get started with RunPod',
  className,
}: {
  children?: ReactNode;
  className?: string;
}) {
  return (
    <>
      <a href={RUNPOD_SIGNUP_URL} target="_blank" rel="noopener noreferrer" className={className}>
        {children}
      </a>{' '}
      <small className="text-xs text-muted-foreground">
        Some provider links are affiliate links.
      </small>
    </>
  );
}
