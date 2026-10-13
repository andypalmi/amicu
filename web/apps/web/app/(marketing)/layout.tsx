import { SiteFooter } from './_components/footer';
import { SiteHeader } from './_components/site-header';

// Marketing shell: our own header and footer (not the Fumadocs HomeLayout,
// which stays on the app shell and /docs). Theme state still comes from the
// root RootProvider (next-themes).
export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <div className="flex flex-1 flex-col">
      <SiteHeader />
      <div data-marketing className="flex flex-1 flex-col">
        {children}
      </div>
      <SiteFooter />
    </div>
  );
}
