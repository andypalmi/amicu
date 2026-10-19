import Link from 'next/link';
import { cn } from '@amicu/ui';
import { faqs, type FaqId, type FaqItem } from '@/app/(marketing)/_lib/faq';
import { JsonLd } from './json-ld';

export function Faq({ ids, className }: { ids?: FaqId[]; className?: string }) {
  const items: FaqItem[] = ids ? ids.map((id) => faqs[id]) : Object.values(faqs);
  return (
    <>
      <JsonLd
        data={{
          '@context': 'https://schema.org',
          '@type': 'FAQPage',
          mainEntity: items.map(({ question, answer, link }) => ({
            '@type': 'Question',
            name: question,
            acceptedAnswer: { '@type': 'Answer', text: link ? `${answer} ${link.label}` : answer },
          })),
        }}
      />
      <dl className={cn('border-b border-border', className)}>
        {items.map(({ question, answer, link }) => (
          <div key={question} className="grid gap-2 border-t border-border py-6 md:grid-cols-3 md:gap-8">
            <dt className="text-lg font-semibold tracking-tight text-foreground">{question}</dt>
            <dd className="max-w-3xl text-base text-muted-foreground md:col-span-2">{answer}
              {link ? (
                <>
                  {' '}
                  <Link
                    href={link.href}
                    className="text-foreground underline underline-offset-4 decoration-border hover:decoration-foreground"
                  >
                    {link.label}
                  </Link>
                </>
              ) : null}
            </dd>
          </div>
        ))}
      </dl>
    </>
  );
}
