import { loader } from 'fumadocs-core/source';
import { defineDocs } from 'fumadocs-mdx/macro';
import { pageSchema } from 'fumadocs-core/source/schema';
import { z } from 'zod';

export const blogRoute = '/blog';

const posts = defineDocs({
  dir: 'content/blog',
  docs: {
    schema: pageSchema.extend({
      /** Publication date, YYYY-MM-DD. */
      date: z.iso.date(),
      author: z.string(),
    }),
  },
});

// Blog posts: flat MDX files under content/blog, served at /blog/<file name>.
export const blog = loader({
  baseUrl: blogRoute,
  source: posts.toFumadocsSource(),
});

export type BlogPost = (typeof blog)['$inferPage'];

/** Newest first. */
export function blogPosts(): BlogPost[] {
  return blog.getPages().sort((a, b) => b.data.date.localeCompare(a.data.date));
}

export function formatPostDate(date: string): string {
  return new Date(`${date}T00:00:00Z`).toLocaleDateString('en-GB', {
    day: 'numeric',
    month: 'long',
    year: 'numeric',
    timeZone: 'UTC',
  });
}
