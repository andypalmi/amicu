'use client';

import { createContext, useContext, type ReactNode } from 'react';
import type { SessionUser, Tier } from '@amicu/auth';

// Carries the server-resolved session (claims only - never the raw token) to
// client components. The value originates from `getSession()` in a Server
// Component and is passed down as a prop, so the browser only ever sees the
// derived `sub`/`tier`/`roles`.

const SessionContext = createContext<SessionUser | null>(null);

export function SessionProvider({
  user,
  children,
}: {
  user: SessionUser | null;
  children: ReactNode;
}) {
  return <SessionContext.Provider value={user}>{children}</SessionContext.Provider>;
}

/** The current session user, or null when unauthenticated. */
export function useSession(): SessionUser | null {
  return useContext(SessionContext);
}

/** Convenience: the current tier, or null when unauthenticated. */
export function useTier(): Tier | null {
  return useContext(SessionContext)?.tier ?? null;
}
