import type { Metadata } from 'next';
import Link from 'next/link';
import { Button, cn } from '@amicu/ui';
import { GAP_HEAD, Headline, HomeSection, MEASURE } from '../_components/home';
import { compareHref, comparePages } from '../_lib/compare';
import { quickstartUrl } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Compare',
  description:
    'How amicu, an open-source CLI for deploying LLMs, compares with Terraform and OpenTofu, SkyPilot, Ollama, and using RunPod or Modal directly.',
  path: '/compare',
});

export default function CompareIndexPage() {
  return (
    <main className="flex flex-1 flex-col">
      <HomeSection>
        <Headline as="h1">Compare amicu</Headline>
        <p className={cn('mt-6 text-base text-muted-foreground sm:text-lg', MEASURE)}>
          amicu is a narrow tool: it deploys vLLM and llama.cpp servers from HCL. These pages say
          where it overlaps with other tools, where it does not, and when to pick each.
        </p>
        <ul className={cn(GAP_HEAD, 'border-b border-border')}>
          {comparePages.map((p) => (
            <li key={p.slug} className="border-t border-border">
              <Link
                href={compareHref(p.slug)}
                className="group flex items-center justify-between gap-6 py-5 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <span>
                  <span className="block text-lg font-medium">{p.title}</span>
                  <span className="mt-1 block text-muted-foreground">{p.summary}</span>
                </span>
                <span
                  aria-hidden
                  className="text-muted-foreground transition-transform motion-safe:group-hover:translate-x-1 group-hover:text-foreground"
                >
                  →
                </span>
              </Link>
            </li>
          ))}
        </ul>
        <div className="mt-8">
          <Button asChild size="lg">
            <Link href={quickstartUrl}>Quickstart</Link>
          </Button>
        </div>
      </HomeSection>
    </main>
  );
}
