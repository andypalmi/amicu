import { RootProvider } from 'fumadocs-ui/provider/next';
import './global.css';
import 'sonner/dist/styles.css';
import { Inter } from 'next/font/google';
import type { Metadata, Viewport } from 'next';
import { Toaster } from '@/components/common/toaster';
import { appName, appTagline, siteUrl } from '@/lib/shared';

const inter = Inter({ subsets: ['latin'] });

export const metadata: Metadata = {
  metadataBase: new URL(siteUrl),
  title: {
    default: appName,
    template: `%s - ${appName}`,
  },
  description: appTagline,
  applicationName: appName,
  openGraph: {
    type: 'website',
    siteName: appName,
    title: appName,
    description: appTagline,
    url: '/',
    images: ['/opengraph-image'],
  },
  twitter: {
    card: 'summary_large_image',
    title: appName,
    description: appTagline,
    images: ['/opengraph-image'],
  },
};

// viewport-fit=cover lets env(safe-area-inset-*) report real insets on notched
// phones (used by the marketing mobile CTA bar).
export const viewport: Viewport = { viewportFit: 'cover' };

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html lang="en" className={inter.className} suppressHydrationWarning>
      <body className="flex flex-col min-h-screen bg-background text-foreground">
        <RootProvider>
          {children}
          <Toaster />
        </RootProvider>
      </body>
    </html>
  );
}
