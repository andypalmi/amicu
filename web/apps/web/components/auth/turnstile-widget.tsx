'use client';

import { useEffect, useRef } from 'react';
import type { TurnstileAction } from '@/lib/security/turnstile';

interface TurnstileApi {
  render: (el: HTMLElement, opts: Record<string, unknown>) => string;
  reset: (id?: string) => void;
  remove: (id?: string) => void;
}
declare global {
  interface Window {
    turnstile?: TurnstileApi;
  }
}

const SCRIPT_SRC = 'https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit';
let scriptPromise: Promise<void> | null = null;

function loadScript(): Promise<void> {
  if (window.turnstile) return Promise.resolve();
  scriptPromise ??= new Promise<void>((resolve, reject) => {
    const s = document.createElement('script');
    s.src = SCRIPT_SRC;
    s.async = true;
    s.onload = () => resolve();
    s.onerror = () => {
      scriptPromise = null;
      reject(new Error('turnstile script failed to load'));
    };
    document.head.appendChild(s);
  });
  return scriptPromise;
}

/**
 * Cloudflare Turnstile widget. Renders nothing when `siteKey` is unset, so dev
 * and un-configured deploys work (the server skips verification in that case
 * too). Turnstile injects a hidden `cf-turnstile-response` input into the
 * enclosing <form>. Tokens are single-use: bump `resetKey` after each action
 * result to obtain a fresh one.
 */
export function TurnstileWidget({
  siteKey,
  action,
  resetKey,
}: {
  siteKey?: string | null;
  action: TurnstileAction;
  resetKey?: unknown;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const idRef = useRef<string | undefined>(undefined);

  useEffect(() => {
    if (!siteKey) return;
    let cancelled = false;
    loadScript()
      .then(() => {
        if (cancelled || !ref.current || !window.turnstile) return;
        idRef.current = window.turnstile.render(ref.current, { sitekey: siteKey, action });
      })
      .catch(() => {
        /* Script blocked/offline: server rejects the submit with a generic error. */
      });
    return () => {
      cancelled = true;
      if (idRef.current) window.turnstile?.remove(idRef.current);
      idRef.current = undefined;
    };
  }, [siteKey, action]);

  useEffect(() => {
    if (resetKey !== undefined && idRef.current) window.turnstile?.reset(idRef.current);
  }, [resetKey]);

  if (!siteKey) return null;
  return <div ref={ref} className="flex justify-center" />;
}
