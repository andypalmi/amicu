import { PageShell } from '@/components/common/page-shell';

// Route-level loading UI for every (app) page: skeleton of the shared page
// header + a content block, shown while the server component resolves the
// session and page data.
export default function Loading() {
  return (
    <PageShell size="lg">
      <div role="status" aria-live="polite" aria-busy="true" className="space-y-6">
        <span className="sr-only">Loading…</span>
        <div className="space-y-2">
          <div className="h-7 w-48 animate-pulse rounded-md bg-muted" />
          <div className="h-4 w-80 max-w-full animate-pulse rounded-md bg-muted" />
        </div>
        <div className="h-48 animate-pulse rounded-xl bg-muted" />
        <div className="h-32 animate-pulse rounded-xl bg-muted" />
      </div>
    </PageShell>
  );
}
