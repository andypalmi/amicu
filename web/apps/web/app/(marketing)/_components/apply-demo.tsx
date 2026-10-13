'use client';

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { cn } from '@amicu/ui';
import {
  DEMO_STEPS,
  demoDuration,
  demoState,
  demoTranscript,
  type Row,
  type Tone,
} from '@/lib/marketing/demo-timeline';

// Server-renders the final frame (t = Infinity); after hydration it rewinds
// to 0 and plays once as soon as it nears the viewport. Reduced motion keeps the final
// frame. Both panes are always laid out (stacked), so height is reserved up
// front and the static frame shows the apply output and the curl call.

const TOTAL = demoDuration(DEMO_STEPS);
const TRANSCRIPT = demoTranscript(DEMO_STEPS);
const START_MARGIN = '200px 0px';
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
    if (!el) return;
    const mql = window.matchMedia('(prefers-reduced-motion: reduce)');
    let io: IntersectionObserver | undefined;
    const stop = () => {
      io?.disconnect();
      cancelAnimationFrame(raf.current);
    };
    const sync = () => {
      stop();
      if (mql.matches) {
        // Keep the final frame, but let the visitor opt in to the motion.
        setT(Infinity);
        setReplayable(true);
        return;
      }
      setT(0);
      io = new IntersectionObserver(
        ([entry]) => {
          // Any overlap with the (expanded) viewport starts the demo; this
          // also holds for short viewports and when already in view on load.
          if (!entry?.isIntersecting) return;
          io?.disconnect();
          play();
        },
        // Start just before the demo scrolls into view.
        { rootMargin: START_MARGIN },
      );
      io.observe(el);
    };
    sync();
    mql.addEventListener('change', sync);
    return () => {
      mql.removeEventListener('change', sync);
      stop();
    };
  }, [play]);

  const state = demoState(DEMO_STEPS, t);
  const term = state.rows.filter((r) => r.screen === 'term');
  const call = state.rows.filter((r) => r.screen === 'call');

  return (
    <div ref={rootRef} className="px-3 py-3 font-mono text-xs leading-5 sm:px-4 sm:text-[0.8125rem] sm:leading-6">
      <div className="flex justify-end">
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
      <pre className="sr-only">{TRANSCRIPT}</pre>
      <div aria-hidden className="mt-3">
        <div>
          {term.map((row, i) => (
            <RowLine key={i} row={row} />
          ))}
        </div>
        <div
          className={cn(
            'mt-3 border-t border-border pt-3',
            state.screen !== 'call' && 'invisible',
          )}
        >
          {call.map((row, i) => (
            <RowLine key={i} row={row} className={i === 1 ? 'mt-3' : undefined} />
          ))}
        </div>
      </div>
    </div>
  );
}
