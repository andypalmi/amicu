import type { Metadata } from 'next';
import Link from 'next/link';
import { redirect } from 'next/navigation';
import { getSession } from '@/lib/auth/session';
import { Brand } from '@/components/common/brand';

// Auth screens carry no indexable content: keep them out of search indexes.
export const metadata: Metadata = {
  robots: { index: false, follow: false },
};

// Public auth surface (/login, /signup, /forgot). Already-authenticated visitors
// are bounced to the app.
export default async function AuthLayout({ children }: LayoutProps<'/'>) {
  const session = await getSession();
  if (session) redirect('/app');

  return (
    <main className="flex flex-1 flex-col items-center justify-center px-6 py-16">
      <div className="w-full max-w-sm">
        <Link
          href="/"
          aria-label="amicu home"
          className="mb-8 flex justify-center rounded-md outline-none transition-opacity hover:opacity-80 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
        >
          <Brand />
        </Link>
        {children}
      </div>
    </main>
  );
}
