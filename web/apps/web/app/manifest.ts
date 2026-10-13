import type { MetadataRoute } from 'next';
import { tokens } from '@/lib/generated/tokens';
import { appName, appTagline } from '@/lib/shared';

export default function manifest(): MetadataRoute.Manifest {
  return {
    name: appName,
    short_name: appName,
    description: appTagline,
    start_url: '/',
    display: 'standalone',
    background_color: tokens.light.background,
    theme_color: tokens.light.background,
    icons: [
      { src: '/icon.svg', sizes: 'any', type: 'image/svg+xml' },
      { src: '/pwa-icon/192', sizes: '192x192', type: 'image/png' },
      { src: '/pwa-icon/512', sizes: '512x512', type: 'image/png' },
      { src: '/pwa-icon/512', sizes: '512x512', type: 'image/png', purpose: 'maskable' },
    ],
  };
}
