import type { Metadata } from 'next';
import Link from 'next/link';
import { CTABand, FactList, PageHeader, Section, SectionHeader } from '../_components/marketing';
import { githubUrl } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

const docsLink = (href: string) => (
  <Link href={href} className="text-foreground underline underline-offset-4">
    Read more in the docs
  </Link>
);

const cliFacts = [
  {
    term: 'Secrets',
    detail: (
      <>
        Write secret(&quot;NAME&quot;) in the config. The value is read from your environment at
        apply. It is never written to state or the plan, and shows as ****** in output.{' '}
        {docsLink('/docs/security/secrets')}
      </>
    ),
  },
  {
    term: 'Endpoint keys',
    detail: (
      <>
        Each endpoint can have its own API key. Locally it goes to the service through an
        owner-only (0600) environment file, never the command line. After apply, amicu sends an
        anonymous request: if it is not rejected, the endpoint is torn down and apply fails.{' '}
        {docsLink('/docs/security/endpoint-auth')}
      </>
    ),
  },
  {
    term: 'State',
    detail: (
      <>
        State is a file in your project, under .amicu/state. You can also keep it in your own
        Postgres when you run the server.{' '}
        {docsLink('/docs/security/state')}
      </>
    ),
  },
  {
    term: 'Telemetry',
    detail: (
      <>
        The CLI sends no usage telemetry. It contacts Modal&apos;s API when you apply to Modal, and
        the serving container downloads weights from Hugging Face. On Modal, your Hugging Face
        token is passed as a Modal secret. Prompts go only to the endpoint you deployed.{' '}
        {docsLink('/docs/security/telemetry')}
      </>
    ),
  },
];

export const metadata: Metadata = pageMetadata({
  title: 'Security',
  description:
    'How the amicu CLI handles secrets, endpoint keys, state and network access, and how to report a vulnerability.',
  path: '/security',
});

export default function SecurityPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Section>
        <PageHeader
          title="Security"
          description="What the CLI does with your secrets, endpoints and data."
        />
        <FactList className="mt-14" items={cliFacts} />
      </Section>

      <Section>
        <SectionHeader title="Hosted console" />
        <p className="mx-auto mt-6 max-w-2xl text-pretty text-center leading-relaxed text-muted-foreground">
          The console is optional. Sessions use short-lived signed tokens and refresh through the
          API. Access rules are enforced by the API, not the browser. Sign-ups are closed.
        </p>
      </Section>

      <Section>
        <CTABand
          title="Report a vulnerability"
          lede="Report it privately through a GitHub security advisory. Please do not file a public issue."
          primary={{ label: 'Report privately', href: `${githubUrl}/security/advisories/new`, external: true }}
          secondary={{ label: 'Security docs', href: '/docs/security' }}
        />
      </Section>
    </main>
  );
}
