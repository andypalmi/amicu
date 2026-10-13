/**
 * Broken internal link check (docs MDX + app source).
 *
 * Uses `next-validate-link` (the Fumadocs-recommended validator) against the
 * real route set: Next.js app routes scanned from `app/` plus every docs page
 * (and its heading anchors) from a Fumadocs `loader()` built over
 * `content/docs`. External links are skipped.
 *
 * Run: `pnpm lint:links` (from `web/` or `web/apps/web`).
 */
import { globSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { getTableOfContents } from 'fumadocs-core/content/toc';
import { loader, type VirtualFile } from 'fumadocs-core/source';
import type { MetaData } from 'fumadocs-core/source';
import { type FileObject, scanURLs, validateFiles } from 'next-validate-link';
import { docsRoute } from '../lib/shared';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const docsDir = path.join(root, 'content/docs');

/** TS/TSX sources whose internal `href`s are checked. */
const SOURCE_GLOBS = ['app/**/*.{ts,tsx}', 'components/**/*.{ts,tsx}', 'lib/layout.shared.tsx'];

/** Makes the validator parse the synthetic Markdown view of a TS/TSX file. */
const SYNTHETIC = '.links.mdx';

const posix = (p: string) => p.split(path.sep).join('/');

function splitFrontmatter(raw: string): { title: string; body: string } {
  const m = /^---\r?\n([\s\S]*?)\r?\n---\r?\n?/.exec(raw);
  const fm = m?.[1] ?? '';
  const title = /^title:\s*["']?(.*?)["']?\s*$/m.exec(fm)?.[1] ?? '';
  return { title, body: m ? raw.slice(m[0].length) : raw };
}

async function loadDocs() {
  const raws = new Map<string, string>();
  const files: VirtualFile[] = [];
  for (const p of globSync('**/*.{md,mdx}', { cwd: docsDir }).map(posix)) {
    const raw = await readFile(path.join(docsDir, p), 'utf8');
    raws.set(p, raw);
    files.push({ type: 'page', path: p, data: { title: splitFrontmatter(raw).title } });
  }
  for (const p of globSync('**/meta.json', { cwd: docsDir }).map(posix)) {
    files.push({
      type: 'meta',
      path: p,
      data: JSON.parse(await readFile(path.join(docsDir, p), 'utf8')) as MetaData,
    });
  }
  return { source: loader({ baseUrl: docsRoute, source: { files } }), raws };
}

/**
 * Internal links in a TS/TSX file, rewritten as a Markdown document that keeps
 * every link on its original line, so reported line numbers match the source.
 */
function tsxToLinkDoc(code: string): string {
  // `href="/x"`, `href={'/x'}`, `href: '/x'`, `url: '/x'`, `` href={`/x?${q}`} ``
  const re = /\b(?:href|url)\s*(?:=\s*\{?|:)\s*(["'`])(\/(?!\/)[^"'`]*)/g;
  return code
    .split('\n')
    .map((line) =>
      Array.from(line.matchAll(re), (m) => {
        // Cut template interpolations: `/wizard?configId=${id}` -> `/wizard`
        const href = m[2].split('${')[0].replace(/[?#&=]+$/, '');
        return `[x](${href})`;
      }).join(' '),
    )
    .join('\n');
}

/**
 * Routes the `next` preset does not see: route handlers (`route.ts`), metadata
 * file conventions (`opengraph-image.tsx`, `robots.ts`, ...) and `public/` files.
 * Dynamic segments are skipped (links to those are not validated).
 */
function extraUrls(): string[] {
  const urls: string[] = [];
  const conventions: Record<string, string> = {
    route: '',
    'opengraph-image': '/opengraph-image',
    'twitter-image': '/twitter-image',
    icon: '/icon',
    'apple-icon': '/apple-icon',
    manifest: '/manifest.webmanifest',
    robots: '/robots.txt',
    sitemap: '/sitemap.xml',
  };
  for (const f of globSync('app/**/*.{ts,tsx,svg,ico}', { cwd: root }).map(posix)) {
    const m = /^app\/(.*?)\/?([^/]+)\.(?:tsx?|svg|ico)$/.exec(f);
    if (!m) continue;
    const dir = m[1].split('/').filter((seg) => seg && !/^\(.*\)$/.test(seg));
    if (dir.some((seg) => seg.startsWith('['))) continue;
    const base = m[2];
    const suffix = base in conventions ? conventions[base] : f.endsWith('favicon.ico') ? '/favicon.ico' : null;
    if (suffix === null) continue;
    urls.push(`/${dir.join('/')}${suffix}`.replace(/\/\//g, '/'));
  }
  for (const f of globSync('public/**/*', { cwd: root }).map(posix)) {
    if (!path.basename(f).startsWith('_')) urls.push(`/${f.slice('public/'.length)}`);
  }
  return urls;
}

async function main() {
  const { source, raws } = await loadDocs();
  const pages = source.getPages();

  const scanned = await scanURLs({
    preset: 'next',
    cwd: root,
    populate: {
      'docs/[[...slug]]': pages.map((page) => ({
        value: { slug: page.slugs },
        hashes: getTableOfContents(splitFrontmatter(raws.get(page.path) ?? '').body).map((item) =>
          item.url.slice(1),
        ),
      })),
    },
  });

  for (const url of extraUrls()) scanned.urls.set(url, {});

  const docFiles: FileObject[] = pages.map((page) => ({
    path: path.join(docsDir, page.path),
    content: raws.get(page.path) ?? '',
    url: page.url,
  }));

  const sourceFiles: FileObject[] = [];
  for (const p of SOURCE_GLOBS.flatMap((g) => globSync(g, { cwd: root }))) {
    const code = await readFile(path.join(root, p), 'utf8');
    sourceFiles.push({ path: `${path.join(root, p)}${SYNTHETIC}`, content: tsxToLinkDoc(code) });
  }

  const results = await validateFiles([...docFiles, ...sourceFiles], {
    scanned,
    markdown: { components: { Card: { attributes: ['href'] } } },
    checkRelativePaths: 'as-url',
  });

  let count = 0;
  for (const r of results) {
    const file = path.relative(root, r.file.replace(SYNTHETIC, ''));
    for (const e of r.errors) {
      count++;
      const reason = e.reason instanceof Error ? e.reason.message : e.reason;
      console.error(`${file}:${e.line}:${e.column}  ${reason}  ${e.url}`);
    }
  }
  if (count > 0) {
    console.error(`\n${count} broken internal link${count === 1 ? '' : 's'}.`);
    process.exit(1);
  }
  console.log(
    `Checked ${docFiles.length} docs pages and ${sourceFiles.length} source files: no broken internal links.`,
  );
}

void main();
