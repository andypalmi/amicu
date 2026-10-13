// Route-level loading UI for /docs while MDX content resolves.
export default function Loading() {
  return (
    <div role="status" aria-live="polite" className="mx-auto w-full max-w-3xl space-y-4 px-6 py-10">
      <span className="sr-only">Loading…</span>
      <div className="h-9 w-2/3 animate-pulse rounded-md bg-muted" />
      <div className="h-4 w-full animate-pulse rounded-md bg-muted" />
      <div className="h-4 w-5/6 animate-pulse rounded-md bg-muted" />
      <div className="h-40 animate-pulse rounded-md bg-muted" />
    </div>
  );
}
