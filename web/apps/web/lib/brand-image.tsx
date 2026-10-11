import { ImageResponse } from 'next/og';
import { brandMarkShapes } from '@/lib/brand-shapes';

// Square monogram PNG on a solid background, for the apple-touch icon and the
// PWA manifest icons. Neutral palette only (no brand accent exists yet).
export function monogramImage(size: number, opts?: { padding?: number }) {
  const pad = opts?.padding ?? Math.round(size * 0.14);
  return new ImageResponse(
    (
      <div
        style={{
          width: '100%',
          height: '100%',
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          background: '#0a0a0a',
          padding: pad,
        }}
      >
        <svg
          viewBox="0 0 24 24"
          width="100%"
          height="100%"
          fill="none"
          stroke="#fafafa"
          strokeWidth={1.75}
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          {brandMarkShapes()}
        </svg>
      </div>
    ),
    { width: size, height: size },
  );
}
