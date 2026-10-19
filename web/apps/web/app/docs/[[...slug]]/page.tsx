import { getPageMarkdownUrl, source } from '@/lib/source';
import {
  DocsBody,
  DocsDescription,
  DocsPage,
  DocsTitle,
  MarkdownCopyButton,
  ViewOptionsPopover,
} from 'fumadocs-ui/layouts/docs/page';
import { notFound } from 'next/navigation';
import { getMDXComponents } from '@/components/mdx';
import type { Metadata } from 'next';
import { createRelativeLink } from 'fumadocs-ui/mdx';
import { JsonLd } from '@/components/seo/json-ld';
import { appName, docsRoute, gitConfig, siteUrl } from '@/lib/shared';
import { formatDate, pageModified } from '@/lib/site-facts';

export default async function Page(props: PageProps<'/docs/[[...slug]]'>) {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  const MDX = page.data.body;
  const markdownUrl = getPageMarkdownUrl(page).url;
  const absoluteUrl = (path: string) => new URL(path, siteUrl).toString();
  const modified = pageModified(`docs/${page.path}`);
  const pageUrl = absoluteUrl(page.url);
  const crumbs = [
    { name: appName, url: siteUrl },
    { name: 'Docs', url: absoluteUrl(docsRoute) },
    ...page.slugs.map((_, i) => source.getPage(page.slugs.slice(0, i + 1))),
  ]
    .map((c) => (c && 'data' in c ? { name: c.data.title, url: absoluteUrl(c.url) } : c))
    .filter((c) => c !== undefined);

  return (
    <DocsPage toc={page.data.toc} full={page.data.full}>
      <JsonLd
        data={[
          {
            '@context': 'https://schema.org',
            '@type': 'TechArticle',
            headline: page.data.title,
            description: page.data.description,
            url: pageUrl,
            mainEntityOfPage: pageUrl,
            ...(modified && { dateModified: modified }),
            author: { '@type': 'Person', name: gitConfig.user, url: `https://github.com/${gitConfig.user}` },
            publisher: { '@type': 'Organization', name: appName, url: siteUrl },
          },
          {
            '@context': 'https://schema.org',
            '@type': 'BreadcrumbList',
            itemListElement: crumbs.map((c, i) => ({
              '@type': 'ListItem',
              position: i + 1,
              name: c.name,
              item: c.url,
            })),
          },
        ]}
      />
      <DocsTitle>{page.data.title}</DocsTitle>
      <DocsDescription className="mb-0">{page.data.description}</DocsDescription>
      <div className="flex flex-row gap-2 items-center border-b pb-6">
        <MarkdownCopyButton markdownUrl={markdownUrl} />
        <ViewOptionsPopover
          markdownUrl={markdownUrl}
          githubUrl={`https://github.com/${gitConfig.user}/${gitConfig.repo}/blob/${gitConfig.branch}/web/apps/web/content/docs/${page.path}`}
        />
      </div>
      <DocsBody>
        <MDX components={getMDXComponents({ a: createRelativeLink(source, page) })} />
      </DocsBody>
      {modified ? (
        <p className="mt-8 text-sm text-fd-muted-foreground">
          Last updated <time dateTime={modified}>{formatDate(modified)}</time>
        </p>
      ) : null}
    </DocsPage>
  );
}

// Unknown slugs must 404 at the routing layer: rendered on demand, notFound()
// runs after loading.tsx has already streamed a 200 shell.
export const dynamicParams = false;

export async function generateStaticParams() {
  return source.generateParams();
}

export async function generateMetadata(props: PageProps<'/docs/[[...slug]]'>): Promise<Metadata> {
  const params = await props.params;
  const page = source.getPage(params.slug);
  if (!page) notFound();

  return {
    title: page.data.title,
    description: page.data.description,
    alternates: { canonical: page.url },
    openGraph: {
      type: 'article',
      siteName: appName,
      title: page.data.title,
      description: page.data.description,
      url: page.url,
    },
  };
}
