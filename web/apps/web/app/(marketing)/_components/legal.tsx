import { notFound } from 'next/navigation';
import type { ReactNode } from 'react';
import { Container } from './marketing';

/** Shared shell for legal pages. */
export function LegalPage({ title, effective, children }: { title: string; effective?: string | null; children: ReactNode }) {
  return (
    <Container className="max-w-3xl py-16">
      <h1 className="text-3xl font-bold tracking-tight">{title}</h1>
      {effective ? <p className="mt-2 text-sm text-muted-foreground">Effective {effective}</p> : null}
      <div className="amicu-prose mt-8 space-y-4 text-sm leading-relaxed [&_h2]:mt-8 [&_h2]:text-xl [&_h2]:font-semibold [&_ul]:list-disc [&_ul]:pl-6">
        {children}
      </div>
    </Container>
  );
}

/** Owner-supplied text, paragraphs split on blank lines. */
export function Body({ text }: { text: string }) {
  return (
    <>
      {text.split(/\n\s*\n/).map((para, i) => (
        <p key={i} className="whitespace-pre-line">
          {para}
        </p>
      ))}
    </>
  );
}

export function requireEnabled(enabled: boolean): void {
  if (!enabled) notFound();
}
