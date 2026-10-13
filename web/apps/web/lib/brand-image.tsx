import { ImageResponse } from 'next/og';

// Square icon PNG for the apple-touch icon and the PWA manifest icons: the
// lowercase "a" of the typed "amicu" wordmark, set in the same semibold weight
// as the header logo, on a solid background. Neutral palette only.
export function monogramImage(size: number) {
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
          color: '#fafafa',
          fontSize: Math.round(size * 0.72),
          fontWeight: 600,
          lineHeight: 1,
          paddingBottom: Math.round(size * 0.06),
        }}
      >
        a
      </div>
    ),
    { width: size, height: size },
  );
}
