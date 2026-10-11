import { monogramImage } from '@/lib/brand-image';

// Fixed-size PNG monogram icons referenced by app/manifest.ts (Chrome needs
// 192 and 512 PNGs for installability) and used to build /favicon.ico (32).
const SIZES = [32, 192, 512];

export function generateStaticParams() {
  return SIZES.map((s) => ({ size: String(s) }));
}

export const dynamicParams = false;

export async function GET(
  _req: Request,
  ctx: { params: Promise<{ size: string }> },
) {
  const { size } = await ctx.params;
  const n = Number(size);
  if (!SIZES.includes(n)) return new Response('Not found', { status: 404 });
  return monogramImage(n);
}
