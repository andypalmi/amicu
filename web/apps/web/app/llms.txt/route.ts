import { source } from '@/lib/source';
import { appName, appTagline, gitConfig, siteUrl } from '@/lib/shared';

export const revalidate = false;

// llms.txt (https://llmstxt.org convention): a compact, machine-readable map of
// the product so generative engines can understand and cite Amicu. The docs
// section is derived from the fumadocs page source map, and llms-full.txt holds
// the concatenated full text of every docs page for deeper retrieval.

function absolute(path: string): string {
  return new URL(path, siteUrl).toString();
}

const marketing: Array<{ path: string; label: string; note: string }> = [
  { path: '/', label: 'Home', note: 'Product overview and getting started.' },
  {
    path: '/pricing',
    label: 'Pricing',
    note: 'Community, Pro, Enterprise, and Hosted tiers with the full feature matrix.',
  },
  {
    path: '/providers',
    label: 'Providers',
    note: 'Where apply works today, planning data for other clouds, and the residency model.',
  },
  {
    path: '/security',
    label: 'Security',
    note: 'Trust model: API-enforced gating, verifiable sessions, secrets by reference.',
  },
  { path: '/docs', label: 'Docs', note: 'Documentation home.' },
];

export function GET() {
  const repoUrl = `https://github.com/${gitConfig.user}/${gitConfig.repo}`;

  const docsLines = source
    .getPages()
    .map((page) => {
      const suffix = page.data.description ? `: ${page.data.description}` : '';
      return `- [${page.data.title}](${absolute(page.url)})${suffix}`;
    })
    .join('\n');

  const marketingLines = marketing
    .map(({ path, label, note }) => `- [${label}](${absolute(path)}): ${note}`)
    .join('\n');

  const body = `# ${appName}

> ${appTagline}

${appName} is an open-source (AGPL-3.0) web client. Planning, recommendations,
policy, and capability logic live in the Amicu Rust Management API; the web app
renders those decisions rather than owning them. Two entry paths: a guided
"help me choose" wizard, and a direct HCL editor for authoring, validating, and
planning declarative serving stacks. Residency-aware by design: apply works
on your own machine and Modal today; other clouds are planning and residency data only.

## Key pages
${marketingLines}

## Documentation
${docsLines}

## Full text for retrieval
- [llms-full.txt](${absolute('/llms-full.txt')}): every documentation page concatenated in full.

## Source
- [GitHub repository](${repoUrl}): the complete open-source monorepo.
`;

  return new Response(body, {
    headers: { 'Content-Type': 'text/plain; charset=utf-8' },
  });
}
