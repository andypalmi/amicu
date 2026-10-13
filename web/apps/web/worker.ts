// Custom Worker entry (wrangler `main`). Wraps the OpenNext-generated worker to
// strip client-controllable forwarding headers before anything reads them:
// OpenNext's request handler copies X-Forwarded-Host into the Host header, which
// would otherwise let a client choose the host that proxy.ts uses for the
// amicu.dev / app.amicu.dev split (and for CSRF origin checks). Cloudflare sets
// the real Host, so these headers are never needed.
//
// The generated file only exists after `opennextjs-cloudflare build`; this file
// is excluded from tsconfig for that reason.
import handler from './.open-next/worker.js';

export { DOQueueHandler, DOShardedTagCache, BucketCachePurge } from './.open-next/worker.js';

const STRIPPED = ['x-forwarded-host', 'x-forwarded-proto'];

const worker = {
  async fetch(request: Request, env: unknown, ctx: unknown): Promise<Response> {
    let req = request;
    if (STRIPPED.some((h) => request.headers.has(h))) {
      const headers = new Headers(request.headers);
      for (const h of STRIPPED) headers.delete(h);
      req = new Request(request, { headers });
    }
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    return (handler as any).fetch(req, env, ctx);
  },
};

export default worker;
