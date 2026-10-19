import facts from './generated/site-facts.json';

const pages: Record<string, string> = facts.pages;

export const softwareVersion: string = facts.version;

/** Last-commit date (ISO 8601) of a marketing route (`/pricing`) or docs file (`docs/quickstart.mdx`). */
export function pageModified(key: string): string | undefined {
  return pages[key];
}

export function formatDate(iso: string): string {
  return new Date(iso).toLocaleDateString('en-US', {
    year: 'numeric',
    month: 'long',
    day: 'numeric',
    timeZone: 'UTC',
  });
}
