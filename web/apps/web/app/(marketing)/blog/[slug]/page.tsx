import type { Metadata } from 'next';
import Link from 'next/link';
import { notFound } from 'next/navigation';
import { getMDXComponents } from '@/components/mdx';
import { blogComponents } from '@/components/blog';
import { JsonLd } from '@/components/seo/json-ld';
import { blog, blogRoute, formatPostDate } from '@/lib/blog';
import { appName, siteUrl } from '@/lib/shared';
import { pageMetadata } from '../../_lib/seo';

export const dynamicParams = false;

export function generateStaticParams() {
  return blog.getPages().map((post) => ({ slug: post.slugs[0] }));
}

export async function generateMetadata({
  params,
}: {
  params: Promise<{ slug: string }>;
}): Promise<Metadata> {
  const post = blog.getPage([(await params).slug]);
  if (!post) return {};
  const meta = pageMetadata({
    title: post.data.title,
    description: post.data.description ?? '',
    path: post.url,
  });
  return {
    ...meta,
    title: { absolute: post.data.title },
    openGraph: { ...meta.openGraph, type: 'article', publishedTime: post.data.date },
  };
}

export default async function BlogPost({ params }: { params: Promise<{ slug: string }> }) {
  const post = blog.getPage([(await params).slug]);
  if (!post) notFound();
  const MDX = post.data.body;
  const url = new URL(post.url, siteUrl).toString();

  return (
    <main className="flex flex-1 flex-col">
      <JsonLd
        data={{
          '@context': 'https://schema.org',
          '@type': 'BlogPosting',
          headline: post.data.title,
          description: post.data.description,
          datePublished: post.data.date,
          url,
          mainEntityOfPage: url,
          author: { '@type': 'Person', name: post.data.author },
          publisher: { '@type': 'Organization', name: appName, url: siteUrl },
        }}
      />
      <article className="mx-auto w-full min-w-0 max-w-2xl px-4 py-16 sm:px-6 sm:py-24">
        <Link
          href={blogRoute}
          className="font-mono text-xs uppercase tracking-wider text-muted-foreground hover:text-foreground"
        >
          ← Blog
        </Link>
        <h1 className="mt-6 text-balance text-4xl font-semibold leading-[1.1] tracking-[-0.03em] sm:text-5xl">
          {post.data.title}
        </h1>
        {post.data.description ? (
          <p className="mt-4 text-lg text-muted-foreground">{post.data.description}</p>
        ) : null}
        <p className="mt-6 font-mono text-xs uppercase tracking-wider text-muted-foreground">
          <time dateTime={post.data.date}>{formatPostDate(post.data.date)}</time> · {post.data.author}
        </p>
        <div className="prose mt-10 max-w-none">
          <MDX components={getMDXComponents(blogComponents)} />
        </div>
      </article>
    </main>
  );
}
