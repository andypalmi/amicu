// Per-page marketing metadata builder.
//
// Centralizes canonical + Open Graph + Twitter wiring so each marketing route
// only supplies its own copy. Relative URLs resolve against `metadataBase`
// (set on the root layout), so no origin is hardcoded here. Pages that set
// their own `openGraph` object would otherwise drop the root defaults (child
// metadata replaces, it does not deep-merge), which is exactly why this helper
// re-applies siteName/type on every page.

import type { Metadata } from 'next';
import { appName } from '@/lib/shared';

interface PageSeo {
  /** Page title; fed through the root title template (`%s - Amicu`). */
  title: string;
  description: string;
  /** Absolute path from the site root, e.g. '/pricing' or '/'. */
  path: string;
}

export function pageMetadata({ title, description, path }: PageSeo): Metadata {
  return {
    title,
    description,
    alternates: { canonical: path },
    openGraph: {
      type: 'website',
      siteName: appName,
      title,
      description,
      url: path,
    },
    twitter: {
      card: 'summary_large_image',
      title,
      description,
    },
  };
}
