import Link from 'next/link';
import { Button } from '@amicu/ui';

export default function LandingPage() {
  return (
    <main className="flex flex-1 flex-col items-center justify-center gap-6 px-6 py-24 text-center">
      <h1 className="max-w-2xl text-4xl font-bold tracking-tight sm:text-5xl">
        Design, deploy, and operate LLM serving stacks.
      </h1>
      <p className="max-w-xl text-lg text-muted-foreground">
        Amicu is a thin, open client over the Rust Management API — guided stack design,
        transparent recommendations, and day-2 operations for EU-first inference.
      </p>
      <div className="flex flex-wrap items-center justify-center gap-3">
        <Button asChild size="lg">
          <Link href="/app">Get started</Link>
        </Button>
        <Button asChild size="lg" variant="outline">
          <Link href="/docs">Read the docs</Link>
        </Button>
      </div>
    </main>
  );
}
