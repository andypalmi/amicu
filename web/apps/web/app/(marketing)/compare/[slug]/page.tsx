import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { CompareView } from '../../_components/compare';
import { comparePages, getComparePage } from '../../_lib/compare';
import { pageMetadata } from '../../_lib/seo';

export const dynamicParams = false;

export function generateStaticParams() {
  return comparePages.map((p) => ({ slug: p.slug }));
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug: string }>;
}): Promise<Metadata> {
  const page = getComparePage((await params).slug);
  if (!page) return {};
  return pageMetadata({
    title: page.title,
    description: page.description,
    path: `/compare/${page.slug}`,
  });
}

export default async function ComparePage({ params }: { params: Promise<{ slug: string }> }) {
  const page = getComparePage((await params).slug);
  if (!page) notFound();
  return <CompareView page={page} />;
}
