'use client';

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { RotateCcw } from 'lucide-react';
import { cn } from '@amicu/ui';
import {
  DEMO_STEPS,
  demoDuration,
  demoState,
  demoTranscript,
  type Row,
  type Tone,
} from '@/lib/marketing/demo-timeline';
import { MockWindow, SCROLLER } from './home';

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
    <span className={cn('block whitespace-pre-wrap', toneClass[row.tone], className)}>
      {row.shown}
      <span className="invisible">{row.rest}</span>
    </span>
  );
}

export function ApplyDemo({ className }: { className?: string }) {
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

  const replay = (
    <button
      type="button"
      onClick={play}
      aria-label="Replay animation"
      title="Replay"
      tabIndex={replayable ? 0 : -1}
      aria-hidden={!replayable}
      className={cn(
        'rounded-sm p-1.5 text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
        !replayable && 'invisible',
      )}
    >
      <RotateCcw aria-hidden className="size-3.5" />
    </button>
  );

  return (
    <MockWindow title="terminal" action={replay} className={className}>
      <div
        ref={rootRef}
        className="grid font-mono text-xs leading-5 sm:text-[0.8125rem] sm:leading-6 lg:grid-cols-2 lg:text-xs lg:leading-[1.125rem]"
      >
        <pre className="sr-only">{TRANSCRIPT}</pre>
        <div aria-hidden className={cn('min-w-0 px-3 py-3 sm:px-4 lg:py-2.5', SCROLLER)}>
          {term.map((row, i) => (
            <RowLine key={i} row={row} />
          ))}
        </div>
        <div aria-hidden className="min-w-0 border-border lg:border-l">
          <div
            className={cn(
              'mx-3 border-t border-border pb-3 pt-3 sm:mx-4 lg:mx-0 lg:border-t-0 lg:px-4 lg:py-2.5',
              SCROLLER,
              state.screen !== 'call' && 'invisible',
            )}
          >
            {call.map((row, i) => (
              <RowLine key={i} row={row} className={i === 1 ? 'mt-3' : undefined} />
            ))}
          </div>
        </div>
      </div>
    </MockWindow>
  );
}
