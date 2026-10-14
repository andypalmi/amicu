import { cn } from '@amicu/ui';

// Shared step indicator for both wizard flows: the classic three-step preset
// wizard (components/wizard/wizard.tsx) and the five-step guided wizard
// (components/wizard/guided/guided-wizard.tsx).
// Takes its labels as a prop since the two flows have a different step count.
export function Stepper({ steps, current }: { steps: readonly string[]; current: number }) {
  return (
    <ol className="flex flex-wrap items-center gap-x-3 gap-y-1 text-sm">
      {steps.map((label, i) => {
        const active = i === current;
        const done = i < current;
        return (
          <li key={label} className="flex items-center gap-2">
            <span
              className={cn(
                'flex h-6 w-6 items-center justify-center rounded-full border text-xs',
                active
                  ? 'border-foreground bg-foreground text-background'
                  : done
                    ? 'border-foreground/40 text-foreground'
                    : 'border-border text-muted-foreground',
              )}
              aria-hidden
            >
              {i + 1}
            </span>
            <span className={active ? 'font-medium' : 'text-muted-foreground'}>{label}</span>
            {i < steps.length - 1 ? (
              <span className="text-muted-foreground" aria-hidden>
                /
              </span>
            ) : null}
          </li>
        );
      })}
    </ol>
  );
}
