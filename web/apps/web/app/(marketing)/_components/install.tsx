'use client';

import { useState } from 'react';
import { cn } from '@amicu/ui';
import { installCommands } from '../_lib/content';
import { CopyButton } from './copy-button';
import { MockWindow } from './home';

/** One command in a window with a copy button. Long lines wrap inside the box. */
export function InstallCommand({
  title,
  command,
  className,
}: {
  title: string;
  command: string;
  className?: string;
}) {
  return (
    <MockWindow title={title} action={<CopyButton />} className={className}>
      <pre className="whitespace-pre-wrap break-all px-4 py-4 font-mono text-xs leading-6 sm:text-[0.8125rem]">
        <code>{command}</code>
      </pre>
    </MockWindow>
  );
}

const order = ['unix', 'brew', 'windows'] as const;

/** Tabbed install commands for the home page: curl first, the others one click away. */
export function InstallTabs({ className }: { className?: string }) {
  const [active, setActive] = useState<(typeof order)[number]>('unix');
  const current = installCommands[active];
  return (
    <div className={className}>
      <div role="tablist" aria-label="Install method" className="mb-3 flex flex-wrap gap-2">
        {order.map((key) => (
          <button
            key={key}
            type="button"
            role="tab"
            id={`install-tab-${key}`}
            aria-selected={active === key}
            aria-controls="install-panel"
            onClick={() => setActive(key)}
            className={cn(
              'rounded-full border px-3 py-1 font-mono text-xs transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
              active === key
                ? 'border-transparent bg-foreground text-background'
                : 'border-border text-muted-foreground hover:bg-accent hover:text-accent-foreground',
            )}
          >
            {installCommands[key].label}
          </button>
        ))}
      </div>
      <div id="install-panel" role="tabpanel" aria-labelledby={`install-tab-${active}`}>
        <InstallCommand title="terminal" command={current.command} />
      </div>
    </div>
  );
}
