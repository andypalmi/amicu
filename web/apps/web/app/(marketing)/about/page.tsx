import type { ReactNode } from 'react';
import type { Metadata } from 'next';
import { gitConfig } from '@/lib/shared';
import { GAP_HEAD, Headline, HomeSection, MEASURE } from '../_components/home';
import { contactUrl, githubUrl, supportedCloudNames } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'About',
  description:
    'amicu is an open-source CLI for deploying vLLM and llama.cpp from HCL. Who maintains it, how it is licensed and how to contribute.',
  path: '/about',
});

const linkClass =
  'text-foreground underline underline-offset-4 decoration-border hover:decoration-foreground';

const facts: Array<{ label: string; value: ReactNode }> = [
  {
    label: 'Project',
    value: (
      <>
        amicu is a command-line tool that deploys vLLM and llama.cpp servers to your own machine
        or to GPU cloud providers ({supportedCloudNames}) from an HCL file, with plan, apply and
        destroy. It is not a hosted service.
      </>
    ),
  },
  {
    label: 'Maintainer',
    value: (
      <a className={linkClass} href={`https://github.com/${gitConfig.user}`}>
        {gitConfig.user}
      </a>
    ),
  },
  {
    label: 'Source',
    value: (
      <a className={linkClass} href={githubUrl}>
        {githubUrl.replace('https://', '')}
      </a>
    ),
  },
  {
    label: 'License',
    value: (
      <>
        AGPL-3.0-only.{' '}
        <a className={linkClass} href={`${githubUrl}/blob/${gitConfig.branch}/LICENSE`}>
          Read the license
        </a>
      </>
    ),
  },
  {
    label: 'Contribute',
    value: (
      <>
        Setup and the checks CI runs are in{' '}
        <a className={linkClass} href={`${githubUrl}/blob/${gitConfig.branch}/CONTRIBUTING.md`}>
          CONTRIBUTING.md
        </a>
        . Report bugs and ideas as{' '}
        <a className={linkClass} href={contactUrl}>
          GitHub issues
        </a>
        , and report vulnerabilities privately as described in{' '}
        <a className={linkClass} href={`${githubUrl}/blob/${gitConfig.branch}/SECURITY.md`}>
          SECURITY.md
        </a>
        .
      </>
    ),
  },
];

export default function AboutPage() {
  return (
    <main className="flex flex-1 flex-col">
      <HomeSection>
        <Headline as="h1">About amicu</Headline>
        <dl className={`${GAP_HEAD} ${MEASURE} border-b border-border`}>
          {facts.map((f) => (
            <div key={f.label} className="border-t border-border py-5">
              <dt className="font-mono text-xs uppercase tracking-wider text-muted-foreground">
                {f.label}
              </dt>
              <dd className="mt-2 text-base text-foreground">{f.value}</dd>
            </div>
          ))}
        </dl>
      </HomeSection>
    </main>
  );
}
