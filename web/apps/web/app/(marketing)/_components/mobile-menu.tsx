'use client';

import { useCallback, useEffect, useId, useRef, useState } from 'react';
import Link from 'next/link';
import { usePathname } from 'next/navigation';
import { ChevronDown, Menu, X } from 'lucide-react';
import { Button, cn } from '@amicu/ui';

export type MobileNavItem = {
  href: string;
  label: string;
  /** Sub-pages; when present the item renders as an inline accordion. */
  children?: { href: string; label: string }[];
};

export type MobileNavCta = {
  href: string;
  label: string;
  variant: 'outline' | 'default';
};

const itemCls =
  'flex w-full items-center justify-between rounded-sm py-3 text-left text-3xl font-medium tracking-tight text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring aria-[current=page]:text-foreground aria-expanded:text-foreground';

/**
 * Full-screen mobile menu (md:hidden). The panel drops from under the header bar, which stays visible: the logo stays put and
 * the toggle becomes an X. CSS transitions only; closes on Esc, link click, route change and when the viewport reaches md.
 */
export function MobileMenu({
  items,
  ctas,
}: {
  items: MobileNavItem[];
  ctas: MobileNavCta[];
}) {
  const [open, setOpen] = useState(false);
  const [expanded, setExpanded] = useState<string | null>(null);
  const panelId = useId();
  const toggleRef = useRef<HTMLButtonElement>(null);
  const pathname = usePathname();

  const close = useCallback((restoreFocus: boolean) => {
    setOpen(false);
    if (restoreFocus) toggleRef.current?.focus();
  }, []);

  // Route change closes the menu (state adjusted during render, not in an effect).
  const [lastPath, setLastPath] = useState(pathname);
  if (lastPath !== pathname) {
    setLastPath(pathname);
    setOpen(false);
  }

  // Reaching md+ closes it (the panel is hidden there anyway).
  useEffect(() => {
    const mq = window.matchMedia('(min-width: 768px)');
    const onChange = () => {
      if (mq.matches) setOpen(false);
    };
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, []);

  // Scroll lock, Esc and focus trap while open.
  useEffect(() => {
    if (!open) return;
    const html = document.documentElement;
    const prev = html.style.overflow;
    html.style.overflow = 'hidden';

    // Everything outside the header (which holds the panel) is inert while open.
    const header = toggleRef.current?.closest('header');
    const outside = Array.from(header?.parentElement?.children ?? []).filter(
      (n): n is HTMLElement => n !== header && n instanceof HTMLElement && !n.inert,
    );
    outside.forEach((n) => (n.inert = true));

    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        close(true);
      }
    };
    document.addEventListener('keydown', onKey);
    return () => {
      html.style.overflow = prev;
      outside.forEach((n) => (n.inert = false));
      document.removeEventListener('keydown', onKey);
    };
  }, [open, close]);

  return (
    <>
      <button
        ref={toggleRef}
        type="button"
        aria-expanded={open}
        aria-controls={panelId}
        aria-label={open ? 'Close menu' : 'Open menu'}
        onClick={() => (open ? close(false) : setOpen(true))}
        className="inline-flex size-9 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring md:hidden"
      >
        {open ? <X className="size-5" aria-hidden /> : <Menu className="size-5" aria-hidden />}
      </button>

      <div
        id={panelId}
        role="dialog"
        aria-modal="true"
        aria-label="Menu"
        className={cn(
          'fixed inset-x-0 bottom-0 top-[calc(3.5rem+1px)] z-40 overflow-y-auto overscroll-contain bg-background md:hidden',
          'transition-[opacity,transform,visibility] duration-200 ease-out motion-reduce:transition-none',
          open ? 'visible translate-y-0 opacity-100' : 'invisible -translate-y-2 opacity-0',
        )}
      >
        <nav aria-label="Mobile" className="mx-auto flex w-full max-w-6xl flex-col px-6 pb-10 pt-6">
          <ul className="flex flex-col gap-1">
            {items.map((item) => {
              if (!item.children) {
                return (
                  <li key={item.href}>
                    <Link
                      href={item.href}
                      aria-current={pathname === item.href ? 'page' : undefined}
                      onClick={() => close(true)}
                      className={itemCls}
                    >
                      {item.label}
                    </Link>
                  </li>
                );
              }
              const isOpen = expanded === item.href;
              const subId = `${panelId}-sub-${item.label}`;
              return (
                <li key={item.href}>
                  <button
                    type="button"
                    aria-expanded={isOpen}
                    aria-controls={subId}
                    onClick={() => setExpanded(isOpen ? null : item.href)}
                    className={itemCls}
                  >
                    {item.label}
                    <ChevronDown
                      aria-hidden
                      className={cn(
                        'size-6 transition-transform duration-200 motion-reduce:transition-none',
                        isOpen && 'rotate-180',
                      )}
                    />
                  </button>
                  <div
                    id={subId}
                    className={cn(
                      'grid transition-[grid-template-rows,visibility] duration-200 ease-out motion-reduce:transition-none',
                      isOpen ? 'visible grid-rows-[1fr]' : 'invisible grid-rows-[0fr]',
                    )}
                  >
                    <ul className="flex flex-col overflow-hidden">
                      {[{ href: item.href, label: 'Overview' }, ...item.children].map((c) => (
                        <li key={c.href}>
                          <Link
                            href={c.href}
                            aria-current={pathname === c.href ? 'page' : undefined}
                            onClick={() => close(true)}
                            className="block rounded-sm py-2 pl-4 text-xl text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring aria-[current=page]:text-foreground"
                          >
                            {c.label}
                          </Link>
                        </li>
                      ))}
                    </ul>
                  </div>
                </li>
              );
            })}
          </ul>

          <div className="mt-10 flex flex-col gap-3">
            {ctas.map((c) => (
              <Button key={c.label} asChild variant={c.variant} size="lg" className="h-12 w-full text-base">
                <Link href={c.href} onClick={() => close(true)}>
                  {c.label}
                </Link>
              </Button>
            ))}
          </div>
        </nav>
      </div>
    </>
  );
}
