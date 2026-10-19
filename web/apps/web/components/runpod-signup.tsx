import type { ReactNode } from 'react';
import { ExternalLink } from 'lucide-react';
import { Button, cn } from '@amicu/ui';
import { RUNPOD_SIGNUP_URL } from '@/app/(marketing)/_lib/content';

export function RunPodSignupLink({
  children = 'Get started with RunPod',
  className,
}: {
  children?: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn('not-prose my-4 flex flex-wrap items-center gap-x-4 gap-y-2', className)}>
      <Button asChild>
        <a href={RUNPOD_SIGNUP_URL} target="_blank" rel="noopener noreferrer">
          {children}
          <ExternalLink aria-hidden className="size-4" />
        </a>
      </Button>
      <small className="text-xs text-muted-foreground">
        Some provider links are affiliate links.
      </small>
    </div>
  );
}
