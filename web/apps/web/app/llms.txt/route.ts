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
  {
    path: '/compare',
    label: 'Compare',
    note: 'amicu compared with Terraform and OpenTofu, SkyPilot, Ollama, and using RunPod or Modal directly.',
  },
  {
    path: '/about',
    label: 'About',
    note: 'The project, its maintainer, license and how to contribute.',
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

${appName} is a command-line tool for deploying open-weight LLM inference. You describe
models, hardware, and deployments in HCL (amicu.hcl). \`amicu validate\` checks
the configuration, \`amicu plan\` shows a diff against recorded state, and
\`amicu apply\` runs llama.cpp or vLLM for you, on your hardware (this machine
or on-prem servers you run amicu on), on Modal, or on RunPod. \`amicu destroy\` stops and
removes the deployments recorded in state, and it confirms each stop before forgetting a
deployment. It is not a hosted service and is not stable yet.

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
