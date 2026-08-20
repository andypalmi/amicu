import Link from 'next/link';
import { redirect } from 'next/navigation';
import { getSession } from '@/lib/auth/session';

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
          className="mb-8 block text-center text-lg font-semibold tracking-tight"
        >
          Amicu
        </Link>
        {children}
      </div>
    </main>
  );
}
