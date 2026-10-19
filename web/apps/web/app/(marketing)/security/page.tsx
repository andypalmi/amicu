import type { Metadata } from 'next';
import Link from 'next/link';
import { Button, cn } from '@amicu/ui';
import { HclCode, Transcript } from '../_components/code';
import {
  FeatureLinks,
  GAP_HEAD,
  Headline,
  HomeSection,
  MEASURE,
  MockWindow,
  ProofLine,
  WindowPair,
} from '../_components/home';
import { githubUrl } from '../_lib/content';
import type { FeatureLink, TermLine } from '../_lib/landing';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Security',
  description:
    'How the amicu CLI handles secrets, endpoint keys, state and network access, and how to report a vulnerability.',
  path: '/security',
});

// Excerpts (trimmed, not full output) following the CLI behaviour documented
// in content/docs/security and the plan renderer in
// crates/amicu-cli/src/commands/plan.rs. Update them if either changes.

const SECRET_HCL = `resource "deployment" "chat" {
  model    = model.qwen.id
  hardware = hardware_profile.gpu.id
  runtime  = "vllm"
  api_key  = secret("CHAT_API_KEY")
}
`;

const SECRET_PLAN: TermLine[] = [
  { text: '$ amicu plan', tone: 'cmd' },
  { text: 'Plan: 1 action(s).' },
  { text: ' ' },
  { text: '  + deployment.chat', tone: 'add' },
  { text: '      action:   create' },
  { text: '      hardware: gpu.l4' },
  { text: '      api_key:  ****** (secret "CHAT_API_KEY")' },
];

const ENDPOINT_CHECK: TermLine[] = [
  { text: 'after apply: GET /v1/models', tone: 'cmd' },
  { text: ' ' },
  { text: '1  no credentials      401/403' },
  { text: '2  gateway only        401/403' },
  { text: '3  Bearer <api_key>    2xx' },
  { text: ' ' },
  { text: 'Open at 1 or 2: endpoint stopped,', tone: 'muted' },
  { text: 'apply fails.', tone: 'muted' },
  { text: 'Unverifiable: not reported as', tone: 'muted' },
  { text: 'deployed.', tone: 'muted' },
];

const LOCAL_HCL = `resource "hardware_profile" "dev" {
  # endpoint binds 127.0.0.1
  provider = "local"
}

resource "deployment" "chat" {
  model    = model.qwen.id
  hardware = hardware_profile.dev.id
  runtime  = "llama_cpp"
  api_key  = secret("LOCAL_API_KEY")
}
`;

const STATE_CONTENTS: TermLine[] = [
  { text: '.amicu/state/state.json', tone: 'cmd' },
  { text: ' ' },
  { text: 'holds', tone: 'muted' },
  { text: '  resource graph from the last apply' },
  { text: '  endpoint URLs, ids, loopback or public' },
  { text: '  secret references (the name)' },
  { text: ' ' },
  { text: 'never holds', tone: 'muted' },
  { text: '  secret values or API keys' },
  { text: '  Modal tokens, proxy tokens, HF_TOKEN' },
  { text: '  prompts or completions' },
];

const NETWORK: TermLine[] = [
  { text: 'connections the CLI makes', tone: 'cmd' },
  { text: ' ' },
  { text: 'validate, fmt, init, doctor', tone: 'muted' },
  { text: '  none' },
  { text: 'plan', tone: 'muted' },
  { text: '  none, or Modal or RunPod if state' },
  { text: '  holds a deployment there' },
  { text: 'apply on this machine', tone: 'muted' },
  { text: '  127.0.0.1:<port>' },
  { text: 'apply on Modal', tone: 'muted' },
  { text: '  api.modal.com:443' },
  { text: '  your Modal endpoint' },
  { text: 'apply on RunPod', tone: 'muted' },
  { text: '  api.runpod.io:443' },
  { text: '  your RunPod endpoint' },
  { text: ' ' },
  { text: 'No telemetry, analytics or update', tone: 'muted' },
  { text: 'checks.', tone: 'muted' },
];

const SECRET_LINKS: FeatureLink[] = [{ label: 'Secrets', href: '/docs/security/secrets' }];
const ENDPOINT_LINKS: FeatureLink[] = [
  { label: 'Endpoint auth', href: '/docs/security/endpoint-auth' },
  { label: 'This machine (local)', href: '/docs/providers/local' },
];
const STATE_LINKS: FeatureLink[] = [
  { label: 'State', href: '/docs/security/state' },
  { label: 'Telemetry and network', href: '/docs/security/telemetry' },
];

const lead = cn('mt-6 text-base text-muted-foreground sm:text-lg', MEASURE);
const advisoryUrl = `${githubUrl}/security/advisories/new`;

export default function SecurityPage() {
  return (
    <main className="flex flex-1 flex-col">
      <HomeSection>
        <Headline as="h1">Security</Headline>
        <p className={lead}>What the CLI does with your secrets, endpoints and data.</p>
        <div className="mt-8 flex flex-wrap items-center gap-3">
          <Button asChild size="lg">
            <a href={advisoryUrl}>Report a vulnerability</a>
          </Button>
          <Button asChild size="lg" variant="outline">
            <Link href="/docs/security">Security docs</Link>
          </Button>
        </div>
      </HomeSection>

      <HomeSection>
        <Headline>Secrets are references, never values.</Headline>
        <p className={lead}>
          A credential field accepts only <code>secret(&quot;NAME&quot;)</code>. A quoted literal is
          a parse error.
        </p>
        <WindowPair
          left={
            <MockWindow title="amicu.hcl">
              <HclCode code={SECRET_HCL} className="[&_pre]:px-4" />
            </MockWindow>
          }
          right={
            <MockWindow title="terminal">
              <Transcript
                lines={SECRET_PLAN}
                wrap
                label="Output of amicu plan showing a redacted api_key"
                className="px-4 py-4"
              />
            </MockWindow>
          }
        />
        <ProofLine
          lead="The value is read from your environment at apply."
          rest="It never reaches the plan, the state or the command line."
        />
        <FeatureLinks links={SECRET_LINKS} />
      </HomeSection>

      <HomeSection>
        <Headline>Endpoints are tested, not trusted.</Headline>
        <p className={lead}>
          After each apply, amicu sends anonymous requests to the new endpoint and expects a
          rejection.
        </p>
        <WindowPair
          left={
            <MockWindow title="endpoint check">
              <Transcript
                lines={ENDPOINT_CHECK}
                label="The three requests of the endpoint check and the answers amicu requires"
                className="px-4 py-4"
              />
            </MockWindow>
          }
          right={
            <MockWindow title="amicu.hcl">
              <HclCode code={LOCAL_HCL} className="[&_pre]:px-4" />
            </MockWindow>
          }
        />
        <ProofLine
          lead="Off-machine deployments require an api_key."
          rest="Local ones bind loopback only, and the key reaches the service through a 0600 file."
        />
        <FeatureLinks links={ENDPOINT_LINKS} />
      </HomeSection>

      <HomeSection>
        <Headline>Your files, your network.</Headline>
        <p className={lead}>State is a file in your project. The CLI sends no telemetry.</p>
        <WindowPair
          left={
            <MockWindow title="state">
              <Transcript
                lines={STATE_CONTENTS}
                wrap
                label="What the state file holds and never holds"
                className="px-4 py-4"
              />
            </MockWindow>
          }
          right={
            <MockWindow title="network">
              <Transcript
                lines={NETWORK}
                label="Network connections the CLI makes"
                className="px-4 py-4"
              />
            </MockWindow>
          }
        />
        <ProofLine
          lead="State still describes your infrastructure,"
          rest="so keep it out of public repositories."
        />
        <FeatureLinks links={STATE_LINKS} />
      </HomeSection>

      <HomeSection>
        <Headline>Found a vulnerability?</Headline>
        <p className={lead}>
          Report it privately through a GitHub security advisory. Please do not file a public
          issue.
        </p>
        <div className={cn('flex flex-wrap items-center gap-3', GAP_HEAD)}>
          <Button asChild size="lg">
            <a href={advisoryUrl}>Report privately</a>
          </Button>
        </div>
      </HomeSection>
    </main>
  );
}
