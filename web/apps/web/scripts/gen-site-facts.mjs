// Writes lib/generated/site-facts.json: the workspace version and the
// last-commit date of each marketing route and docs page. Needs full history;
// on a shallow clone the committed file is left untouched.
import { execFileSync } from 'node:child_process';
import { globSync, readFileSync, writeFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const out = path.join(root, 'lib/generated/site-facts.json');

const vcs = (args) => execFileSync('git', args, { cwd: root, encoding: 'utf8' }).trim();

if (vcs(['rev-parse', '--is-shallow-repository']) === 'true') {
  console.log('shallow clone: keeping committed site-facts.json');
  process.exit(0);
}

const lastCommit = (files) =>
  files
    .map((f) => vcs(['log', '-1', '--format=%cI', '--', f]))
    .filter(Boolean)
    .sort()
    .at(-1);

const marketing = {
  '/': [
    'app/(marketing)/page.tsx',
    'app/(marketing)/_lib/landing.ts',
    'app/(marketing)/_lib/faq.ts',
    'app/(marketing)/_lib/content.ts',
  ],
  '/about': ['app/(marketing)/about/page.tsx', 'app/(marketing)/_lib/content.ts'],
  '/install': ['app/(marketing)/install/page.tsx'],
  '/pricing': ['app/(marketing)/pricing/page.tsx'],
  '/providers': ['app/(marketing)/providers/page.tsx', 'app/(marketing)/_lib/content.ts'],
  '/security': ['app/(marketing)/security/page.tsx'],
  '/privacy': ['app/(marketing)/privacy/page.tsx'],
  '/terms': ['app/(marketing)/terms/page.tsx'],
  '/imprint': ['app/(marketing)/imprint/page.tsx'],
};

const pages = {};
for (const [route, files] of Object.entries(marketing)) {
  const date = lastCommit(files);
  if (date) pages[route] = date;
}
for (const file of globSync('**/*.{md,mdx}', { cwd: path.join(root, 'content/docs') }).sort()) {
  const date = lastCommit([`content/docs/${file}`]);
  if (date) pages[`docs/${file.split(path.sep).join('/')}`] = date;
}

const cargo = readFileSync(path.join(root, '../../../Cargo.toml'), 'utf8');
const version = /^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m.exec(cargo)?.[1];
if (!version) throw new Error('workspace version not found in Cargo.toml');

writeFileSync(out, `${JSON.stringify({ version, pages }, null, 2)}\n`);
