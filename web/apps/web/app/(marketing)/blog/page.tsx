import type { Metadata } from 'next';
import Link from 'next/link';
import { blogPosts, formatPostDate } from '@/lib/blog';
import { GAP_HEAD, Headline, HomeSection, MEASURE } from '../_components/home';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Blog',
  description: 'Notes on running open LLMs: memory, hardware and deployment, with the numbers worked out.',
  path: '/blog',
});

export default function BlogIndex() {
  const posts = blogPosts();
  return (
    <main className="flex flex-1 flex-col">
      <HomeSection>
        <Headline as="h1">Blog</Headline>
        <ul className={`${GAP_HEAD} ${MEASURE} border-b border-border`}>
          {posts.map((post) => (
            <li key={post.url} className="border-t border-border py-6">
              <Link
                href={post.url}
                className="group block rounded-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <time
                  dateTime={post.data.date}
                  className="font-mono text-xs uppercase tracking-wider text-muted-foreground"
                >
                  {formatPostDate(post.data.date)}
                </time>
                <h2 className="mt-2 text-xl font-semibold tracking-tight text-foreground group-hover:underline group-hover:underline-offset-4">
                  {post.data.title}
                </h2>
                {post.data.description ? (
                  <p className="mt-2 text-base text-muted-foreground">{post.data.description}</p>
                ) : null}
              </Link>
            </li>
          ))}
        </ul>
      </HomeSection>
    </main>
  );
}
