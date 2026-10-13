/**
 * Post-login redirect target: only local, absolute in-app paths. Rejects
 * protocol-relative (`//x`), backslash tricks (`/\x`, which browsers treat as
 * `//x`), control characters, and anything that does not resolve to the same
 * origin. Falls back to `fallback`.
 */
export function safeNextPath(raw: unknown, fallback: string): string {
  const value = typeof raw === 'string' ? raw : '';
  if (!value.startsWith('/') || value.startsWith('//')) return fallback;
  // eslint-disable-next-line no-control-regex
  if (/[\\\u0000-\u001f\u007f]/.test(value)) return fallback;
  try {
    const u = new URL(value, 'http://safe.invalid');
    if (u.origin !== 'http://safe.invalid') return fallback;
  } catch {
    return fallback;
  }
  return value;
}
