'use client';

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { cn } from '@amicu/ui';
import {
  DEMO_STEPS,
  demoDuration,
  demoState,
  type Row,
  type Status,
  type Tone,
} from '@/lib/marketing/demo-timeline';

// Server-renders the final frame (t = Infinity); after hydration it rewinds
// to 0 and plays once when scrolled into view. Reduced motion keeps the final
// frame. Both screens share one grid cell, so height is reserved up front.

const TOTAL = demoDuration(DEMO_STEPS);
const toneClass: Record<Tone, string> = {
  cmd: 'text-accent-text',
  add: 'text-success',
  muted: 'text-muted-foreground',
  plain: 'text-foreground',
};
const useIsoLayoutEffect = typeof window === 'undefined' ? useEffect : useLayoutEffect;

function RowLine({ row, className }: { row: Row; className?: string }) {
  return (
    <span className={cn('block whitespace-pre-wrap break-words', toneClass[row.tone], className)}>
      {row.shown}
      <span className="invisible">{row.rest}</span>
    </span>
  );
}

function StatusChip({ status }: { status: Status | null }) {
  return (
    <span
      className={cn(
        'inline-flex min-w-24 items-center justify-center gap-1.5 rounded-full border px-2.5 py-0.5 text-xs',
        status === 'online' ? 'border-success text-success' : 'border-border text-muted-foreground',
        !status && 'invisible',
      )}
    >
      <span aria-hidden className="size-1.5 rounded-full bg-current" />
      {status ?? 'starting'}
    </span>
  );
}

export function ApplyDemo() {
  const [t, setT] = useState(Infinity);
  const [replayable, setReplayable] = useState(false);
  const rootRef = useRef<HTMLDivElement>(null);
  const raf = useRef(0);

  const play = useCallback(() => {
    cancelAnimationFrame(raf.current);
    setReplayable(false);
    const t0 = performance.now();
    const tick = (now: number) => {
      const elapsed = now - t0;
      if (elapsed >= TOTAL) {
        setT(Infinity);
        setReplayable(true);
        return;
      }
      setT(elapsed);
      raf.current = requestAnimationFrame(tick);
    };
    setT(0);
    raf.current = requestAnimationFrame(tick);
  }, []);

  useIsoLayoutEffect(() => {
    const el = rootRef.current;
    if (!el || window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    setT(0);
    const io = new IntersectionObserver(
      ([entry]) => {
        if (!entry?.isIntersecting) return;
        io.disconnect();
        play();
      },
      { threshold: 0.4 },
    );
    io.observe(el);
    return () => {
      io.disconnect();
      cancelAnimationFrame(raf.current);
    };
  }, [play]);

  const state = demoState(DEMO_STEPS, t);
  const term = state.rows.filter((r) => r.screen === 'term');
  const call = state.rows.filter((r) => r.screen === 'call');

  return (
    <div ref={rootRef} className="px-4 py-3 font-mono text-[0.8125rem] leading-6">
      <div className="flex items-center justify-between">
        <StatusChip status={state.status} />
        <button
          type="button"
          onClick={play}
          className={cn(
            'rounded-sm px-1 text-xs text-muted-foreground underline-offset-4 hover:text-foreground hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
            !replayable && 'invisible',
          )}
          tabIndex={replayable ? 0 : -1}
        >
          Replay
        </button>
      </div>
      <div className="mt-3 grid">
        <div
          aria-label="Output of amicu apply"
          role="img"
          className={cn('col-start-1 row-start-1', state.screen !== 'term' && 'invisible')}
          aria-hidden={state.screen !== 'term'}
        >
          {term.map((row, i) => (
            <RowLine key={i} row={row} />
          ))}
        </div>
        <div
          aria-label="A curl request to the endpoint and its response"
          role="img"
          aria-hidden={state.screen !== 'call'}
          className={cn('col-start-1 row-start-1', state.screen !== 'call' && 'invisible')}
        >
          {call.map((row, i) => (
            <RowLine key={i} row={row} className={i === 1 ? 'mt-3' : undefined} />
          ))}
        </div>
      </div>
    </div>
  );
}
