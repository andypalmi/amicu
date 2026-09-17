import { cn } from '@amicu/ui';

// The guided wizard's step indicator (andypalmi/amicu-planning#85), adapted
// from the `Stepper` in components/wizard/wizard.tsx to take its labels as a
// prop instead of a module-level constant, since `/new/guided` has five steps
// (not the classic wizard's three).
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
