import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { baseOptions } from '@/lib/layout.shared';

// Authed product surface. Route protection + session wiring land with amicu-planning#78.
export default function Layout({ children }: LayoutProps<'/'>) {
  return <HomeLayout {...baseOptions()}>{children}</HomeLayout>;
}
