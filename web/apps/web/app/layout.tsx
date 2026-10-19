import { RootProvider } from 'fumadocs-ui/provider/next';
import './global.css';
import 'sonner/dist/styles.css';
import { Martian_Mono, Schibsted_Grotesk } from 'next/font/google';
import type { Metadata, Viewport } from 'next';
import { Analytics } from '@/components/analytics/analytics';
import { CloudflareBeacon } from '@/components/analytics/cf-beacon';
import { Toaster } from '@/components/common/toaster';
import { JsonLd } from '@/components/seo/json-ld';
import { tokens } from '@/lib/generated/tokens';
import { appName, appTagline, gitConfig, siteUrl } from '@/lib/shared';

const schibsted = Schibsted_Grotesk({
  subsets: ['latin'],
  variable: '--font-schibsted',
  display: 'swap',
});
const martian = Martian_Mono({
  subsets: ['latin'],
  variable: '--font-martian',
  display: 'swap',
});

// Matches --background (light / dark). Follows the OS preference; the in-app
// toggle (next-themes) only changes the page itself.
export const viewport: Viewport = {
  colorScheme: 'light dark',
  themeColor: [
    { media: '(prefers-color-scheme: light)', color: tokens.light.background },
    { media: '(prefers-color-scheme: dark)', color: tokens.dark.background },
  ],
};

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

const orgJsonLd = {
  '@context': 'https://schema.org',
  '@type': 'Organization',
  name: appName,
  url: siteUrl,
  logo: `${siteUrl}/icon.svg`,
  description: appTagline,
  sameAs: [`https://github.com/${gitConfig.user}/${gitConfig.repo}`],
};

const siteJsonLd = {
  '@context': 'https://schema.org',
  '@type': 'WebSite',
  name: appName,
  url: siteUrl,
  description: appTagline,
};

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html lang="en" className={`${schibsted.variable} ${martian.variable}`} suppressHydrationWarning>
      <body className="flex flex-col min-h-screen bg-background text-foreground">
        <JsonLd data={[orgJsonLd, siteJsonLd]} />
        <RootProvider>
          {children}
          <Toaster />
          <Analytics />
          <CloudflareBeacon />
        </RootProvider>
      </body>
    </html>
  );
}
