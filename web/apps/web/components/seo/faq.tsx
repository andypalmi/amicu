import { cn } from '@amicu/ui';
import { faqs, type FaqId } from '@/app/(marketing)/_lib/faq';
import { JsonLd } from './json-ld';

export function Faq({ ids, className }: { ids?: FaqId[]; className?: string }) {
  const items = ids ? ids.map((id) => faqs[id]) : Object.values(faqs);
  return (
    <>
      <JsonLd
        data={{
          '@context': 'https://schema.org',
          '@type': 'FAQPage',
          mainEntity: items.map(({ question, answer }) => ({
            '@type': 'Question',
            name: question,
            acceptedAnswer: { '@type': 'Answer', text: answer },
          })),
        }}
      />
      <dl className={cn('border-b border-border', className)}>
        {items.map(({ question, answer }) => (
          <div key={question} className="border-t border-border py-6">
            <dt className="text-lg font-semibold tracking-tight text-foreground">{question}</dt>
            <dd className="mt-2 max-w-3xl text-base text-muted-foreground">{answer}</dd>
          </div>
        ))}
      </dl>
    </>
  );
}
