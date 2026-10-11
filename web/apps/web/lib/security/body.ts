// Size-capped JSON body reader for BFF route handlers. Rejects on the declared
// Content-Length first, then enforces the cap while streaming (chunked or lying
// clients), so an oversized body is never fully buffered.

/** Default cap: 1 MiB. Configs/answers are small text; this is generous. */
export const MAX_BODY_BYTES = 1024 * 1024;

export type JsonBody = { value: unknown } | { deny: Response };

export async function readJsonBody(
  request: Request,
  maxBytes: number = MAX_BODY_BYTES,
): Promise<JsonBody> {
  const tooLarge = (): JsonBody => ({
    deny: Response.json({ error: 'request body too large' }, { status: 413 }),
  });
  const notJson = (): JsonBody => ({
    deny: Response.json({ error: 'request body must be JSON' }, { status: 400 }),
  });

  const declared = request.headers.get('content-length');
  if (declared !== null) {
    const n = Number(declared);
    if (Number.isFinite(n) && n > maxBytes) return tooLarge();
  }

  if (!request.body) return notJson();
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let total = 0;
  try {
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      total += value.byteLength;
      if (total > maxBytes) {
        await reader.cancel().catch(() => {});
        return tooLarge();
      }
      chunks.push(value);
    }
  } catch {
    return notJson();
  }
  const buf = new Uint8Array(total);
  let off = 0;
  for (const c of chunks) {
    buf.set(c, off);
    off += c.byteLength;
  }
  try {
    return { value: JSON.parse(new TextDecoder().decode(buf)) };
  } catch {
    return notJson();
  }
}
