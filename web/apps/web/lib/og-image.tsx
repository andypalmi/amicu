import { ImageResponse } from 'next/og';

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
        <div style={{ fontSize: 56, fontWeight: 600 }}>amicu</div>
        <div style={{ display: 'flex', flexDirection: 'column', gap: 24 }}>
          <div style={{ fontSize: 76, fontWeight: 700, lineHeight: 1.05, letterSpacing: -2 }}>
            Infrastructure as code for self-hosted LLMs.
          </div>
          <div style={{ fontSize: 30, color: '#a3a3a3' }}>
            Describe the model, hardware and runtime in HCL. Plan, then apply.
          </div>
        </div>
      </div>
    ),
    ogSize,
  );
}
