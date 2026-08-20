import { RootProvider } from 'fumadocs-ui/provider/next';
import './global.css';
import { Inter } from 'next/font/google';
import type { Metadata } from 'next';

const inter = Inter({ subsets: ['latin'] });

export const metadata: Metadata = {
  title: {
    default: 'Amicu',
    template: '%s — Amicu',
  },
  description: 'A thin client over the Amicu Rust Management API.',
};

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <html lang="en" className={inter.className} suppressHydrationWarning>
      <body className="flex flex-col min-h-screen bg-background text-foreground">
        <RootProvider>{children}</RootProvider>
      </body>
    </html>
  );
}
