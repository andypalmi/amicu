import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { baseOptions } from '@/lib/layout.shared';

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <HomeLayout
      {...baseOptions()}
      links={[
        { text: 'Pricing', url: '/pricing' },
        { text: 'Providers', url: '/providers' },
        { text: 'Security', url: '/security' },
        { text: 'Docs', url: '/docs' },
        { text: 'App', url: '/app' },
      ]}
    >
      {children}
    </HomeLayout>
  );
}
