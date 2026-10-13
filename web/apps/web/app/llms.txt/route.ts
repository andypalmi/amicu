import { source } from '@/lib/source';
import { appName, appTagline, gitConfig, siteUrl } from '@/lib/shared';

export const revalidate = false;

// llms.txt (https://llmstxt.org convention): a compact, machine-readable map of
// the product so generative engines can understand and cite amicu. The docs
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
    note: 'The CLI is free and open source (AGPL-3.0); team and enterprise plans are planned.',
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

${appName} is a command-line tool for self-hosted LLM inference. You describe
models, hardware, and deployments in HCL (amicu.hcl); \`amicu validate\` checks
the configuration (structure, references, hardware fit, data residency, policy),
\`amicu plan\` shows a diff against recorded state, and \`amicu apply\` runs
llama.cpp or vLLM for you, on your own machine (launchd or systemd user
service) or on Modal (serverless GPU, pinned region). Same workflow as
Terraform: declarative config, a plan you read first, and state. Credentials
stay out of configuration, plans, and state. It is not a hosted service, a
model registry, or a gateway, and it is not stable yet: expect breaking
changes. Other providers (Hetzner, OVHcloud, Scaleway, RunPod, SSH hosts) are
previews; AWS, Azure, GCP, and Nebius have planning and residency data only.

Start with the [Introduction](${absolute('/docs')}) and the
[Quickstart](${absolute('/docs/quickstart')}).

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
