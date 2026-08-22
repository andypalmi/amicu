import type { Metadata } from 'next';
import Link from 'next/link';
import {
  Building2,
  Eye,
  KeyRound,
  Lock,
  MapPin,
  ShieldCheck,
} from 'lucide-react';
import {
  Button,
  Card,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@amicu/ui';
import { Container, PageHeader } from '../_components/marketing';
import { pageMetadata } from '../_lib/seo';

const pillars = [
  {
    icon: ShieldCheck,
    title: 'Enforced by the API, not the UI',
    body: 'The frontend mirrors tier and role gating for UX — show, lock, upsell — but never enforces it. The Rust Management API re-checks every mutation server-side. Reading a claim never grants an action.',
  },
  {
    icon: KeyRound,
    title: 'Asymmetric, verifiable sessions',
    body: 'Auth issues asymmetric JWTs with a published JWKS at /.well-known/jwks.json. Tier and role claims verify offline; sessions refresh through the API. No third-party identity dependency in the open core.',
  },
  {
    icon: Lock,
    title: 'Secrets by reference',
    body: 'Amicu works with credential and secret references, not copies of your keys. Provider credentials stay in your secret store; the stack declares what to use, not the secret itself.',
  },
  {
    icon: MapPin,
    title: 'Data residency you choose',
    body: 'Residency and region are first-class planning inputs. Deploy on EU-native providers, hyperscaler EU regions, or your own hardware over SSH for complete residency control.',
  },
  {
    icon: Eye,
    title: 'Open source, auditable',
    body: 'The entire frontend — marketing, app, and docs — is AGPL-3.0 in the open monorepo. There is no closed UI hiding paid features; anyone can read and audit exactly what runs in the browser.',
  },
  {
    icon: Building2,
    title: 'Enterprise governance',
    body: 'Org administration, RBAC, SSO, and audit are available on the Enterprise tier for teams that need centralized access control and a verifiable record of who changed what.',
  },
];

export const metadata: Metadata = pageMetadata({
  title: 'Security',
  description:
    'How Amicu handles trust: API-enforced tier gating, asymmetric verifiable sessions, secrets by reference, EU data residency, and a fully open-source auditable frontend.',
  path: '/security',
});

export default function SecurityPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Container className="py-20 sm:py-24">
        <PageHeader
          eyebrow="Security"
          title="Trust the boundary, not the browser."
          description="Amicu is a thin client. Planning, policy, and capability logic live in the API — and so does enforcement. The web app renders decisions, it never makes the ones that guard your infrastructure."
        />

        <div className="mt-14 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
          {pillars.map((pillar) => (
            <Card key={pillar.title}>
              <CardHeader className="gap-3">
                <pillar.icon className="size-6 text-primary" aria-hidden />
                <CardTitle className="text-lg">{pillar.title}</CardTitle>
                <CardDescription>{pillar.body}</CardDescription>
              </CardHeader>
            </Card>
          ))}
        </div>

        <section className="mt-16 rounded-xl border border-border bg-muted/30 p-8">
          <h2 className="text-xl font-semibold tracking-tight">
            The thin-client boundary
          </h2>
          <p className="mt-2 max-w-3xl text-sm text-muted-foreground">
            The frontend reads <code className="text-foreground">tier</code> and{' '}
            <code className="text-foreground">roles</code> claims to decide what to show
            or lock — but it never trusts them for enforcement. Every write goes to the
            API, which verifies the session and re-checks authorization before acting.
            Compromising the browser cannot grant capabilities the server did not.
          </p>
          <div className="mt-6">
            <Button asChild variant="outline">
              <Link href="/docs">Read the security docs</Link>
            </Button>
          </div>
        </section>
      </Container>
    </main>
  );
}
