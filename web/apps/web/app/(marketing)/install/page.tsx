import type { Metadata } from 'next';
import Link from 'next/link';
import { Button, cn } from '@amicu/ui';
import { Headline, HomeSection, MEASURE } from '../_components/home';
import { InstallCommand } from '../_components/install';
import { installCommands, quickstartUrl, releasesUrl } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Install',
  description:
    'Install the amicu CLI on macOS, Linux or Windows with the install script or Homebrew, then check the version.',
  path: '/install',
});

const lead = cn('mt-6 text-base text-muted-foreground sm:text-lg', MEASURE);
const h3 = 'mb-3 text-lg font-semibold tracking-tight';

export default function InstallPage() {
  return (
    <main className="flex flex-1 flex-col">
      <HomeSection>
        <Headline as="h1">Install amicu</Headline>
        <p className={lead}>
          macOS and Linux (arm64 and x86_64), and Windows (x86_64). The installer downloads a
          prebuilt binary and puts <code>amicu</code> in <code>~/.local/bin</code>. It edits your
          shell profile, so open a new shell before running it.
        </p>

        <div className="mt-12 grid max-w-3xl gap-10">
          <section>
            <h2 className={h3}>macOS and Linux</h2>
            <InstallCommand title="terminal" command={installCommands.unix.command} />
          </section>
          <section>
            <h2 className={h3}>Homebrew</h2>
            <InstallCommand title="terminal" command={installCommands.brew.command} />
          </section>
          <section>
            <h2 className={h3}>Windows (PowerShell)</h2>
            <InstallCommand title="powershell" command={installCommands.windows.command} />
          </section>
          <section>
            <h2 className={h3}>Verify</h2>
            <InstallCommand title="terminal" command="amicu --version" />
          </section>
        </div>
      </HomeSection>

      <HomeSection>
        <Headline>Next step</Headline>
        <p className={lead}>
          Describe a model, plan it and apply it. Prefer a manual download? Every release has
          prebuilt binaries.
        </p>
        <div className="mt-8 flex flex-wrap items-center gap-3">
          <Button asChild size="lg">
            <Link href={quickstartUrl}>Read the quickstart</Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <a href={releasesUrl}>GitHub releases</a>
          </Button>
        </div>
      </HomeSection>
    </main>
  );
}
