import { ImageResponse } from 'next/og';
import { appName } from '@/lib/shared';
import { brandMarkShapes } from '@/lib/brand-shapes';

export const ogSize = { width: 1200, height: 630 };

// Shared 1200x630 social card for opengraph-image and twitter-image. Neutral
// monochrome, system fonts only (ImageResponse falls back to its bundled sans).
export function ogImage() {
  return new ImageResponse(
    (
      <div
        style={{
          width: '100%',
          height: '100%',
          display: 'flex',
          flexDirection: 'column',
          justifyContent: 'space-between',
          background: '#0a0a0a',
          color: '#fafafa',
          padding: 72,
        }}
      >
        <div style={{ display: 'flex', alignItems: 'center', gap: 20 }}>
          <svg
            viewBox="0 0 24 24"
            width={64}
            height={64}
            fill="none"
            stroke="#fafafa"
            strokeWidth={1.75}
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            {brandMarkShapes()}
          </svg>
          <div style={{ fontSize: 44, fontWeight: 600 }}>{appName}</div>
        </div>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 24 }}>
          <div style={{ fontSize: 76, fontWeight: 700, lineHeight: 1.05, letterSpacing: -2 }}>
            Design, deploy, and operate LLM serving stacks.
          </div>
          <div style={{ fontSize: 30, color: '#a3a3a3' }}>
            Open-source infrastructure as code for self-hosted LLMs.
          </div>
        </div>
      </div>
    ),
    ogSize,
  );
}
