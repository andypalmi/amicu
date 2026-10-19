import Link from 'next/link';
import { Button, cn } from '@amicu/ui';
import type { ComparePage } from '../_lib/compare';
import { quickstartUrl } from '../_lib/content';
import { GAP_HEAD, Headline, HomeSection, MEASURE, SCROLLER } from './home';

const lead = cn('mt-6 text-base text-muted-foreground sm:text-lg', MEASURE);

function JsonLd({ data }: { data: object }) {
  return (
    <script
      type="application/ld+json"
      dangerouslySetInnerHTML={{ __html: JSON.stringify(data).replace(/</g, '\\u003c') }}
    />
  );
}

function BulletList({ items }: { items: string[] }) {
  return (
    <ul className="mt-6 divide-y divide-border border-y border-border">
      {items.map((item) => (
        <li key={item} className="py-3 text-base text-muted-foreground">
          {item}
        </li>
      ))}
    </ul>
  );
}

export function CompareView({ page }: { page: ComparePage }) {
  const faqLd = {
    '@context': 'https://schema.org',
    '@type': 'FAQPage',
    mainEntity: page.faq.map((f) => ({
      '@type': 'Question',
      name: f.q,
      acceptedAnswer: { '@type': 'Answer', text: f.a },
    })),
  };

  return (
    <main className="flex flex-1 flex-col">
      <JsonLd data={faqLd} />
      <HomeSection>
        <Headline as="h1">{page.headline}</Headline>
        <p className={lead}>{page.verdict}</p>
        <div className="mt-8 flex flex-wrap items-center gap-3">
          <Button asChild size="lg">
            <Link href={quickstartUrl}>Quickstart</Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <Link href="/compare">All comparisons</Link>
          </Button>
        </div>
      </HomeSection>

      <HomeSection>
        <Headline>Side by side</Headline>
        <div className={cn(GAP_HEAD, SCROLLER)}>
          <table className="w-full min-w-[40rem] border-collapse text-left text-sm">
            <caption className="sr-only">{page.title}</caption>
            <thead>
              <tr className="border-b border-border">
                <th scope="col" className="w-1/5 py-3 pr-4 font-medium text-muted-foreground">
                  &nbsp;
                </th>
                <th scope="col" className="w-2/5 py-3 pr-4 font-semibold">
                  {page.other}
                </th>
                <th scope="col" className="w-2/5 py-3 font-semibold">
                  amicu
                </th>
              </tr>
            </thead>
            <tbody>
              {page.rows.map((row) => (
                <tr key={row.aspect} className="border-b border-border align-top">
                  <th scope="row" className="py-3 pr-4 font-medium">
                    {row.aspect}
                  </th>
                  <td className="py-3 pr-4 text-muted-foreground">{row.other}</td>
                  <td className="py-3 text-muted-foreground">{row.amicu}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <p className={cn('mt-6 text-sm text-muted-foreground', MEASURE)}>
          Sources for the {page.other} column:{' '}
          {page.sources.map((s, i) => (
            <span key={s.href}>
              {i > 0 ? ', ' : ''}
              <a className="underline hover:text-foreground" href={s.href}>
                {s.label}
              </a>
            </span>
          ))}
          . Details can change; check the official documentation.
        </p>
      </HomeSection>

      <HomeSection>
        <div className="grid gap-12 lg:grid-cols-2">
          <div>
            <Headline>{`When to choose ${page.other}`}</Headline>
            <BulletList items={page.chooseOther} />
          </div>
          <div>
            <Headline>When to choose amicu</Headline>
            <BulletList items={page.chooseAmicu} />
          </div>
        </div>
      </HomeSection>

      <HomeSection>
        <Headline>Questions</Headline>
        <dl className={cn(GAP_HEAD, 'border-b border-border', MEASURE)}>
          {page.faq.map((f) => (
            <div key={f.q} className="border-t border-border py-5">
              <dt className="font-medium">{f.q}</dt>
              <dd className="mt-2 text-muted-foreground">{f.a}</dd>
            </div>
          ))}
        </dl>
      </HomeSection>

      <HomeSection>
        <Headline>Try amicu</Headline>
        <p className={lead}>Install the CLI and deploy a model on your machine or a GPU provider.</p>
        <div className="mt-8 flex flex-wrap items-center gap-3">
          <Button asChild size="lg">
            <Link href={quickstartUrl}>Quickstart</Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <Link href="/providers">Providers</Link>
          </Button>
        </div>
      </HomeSection>
    </main>
  );
}
