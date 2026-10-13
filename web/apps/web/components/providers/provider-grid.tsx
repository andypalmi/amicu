import { providers } from '@/app/(marketing)/_lib/content';
import { ProviderRegistry } from './provider-registry';

/** Server wrapper so MDX can render the registry without props. */
export function ProviderGrid() {
  return <ProviderRegistry items={providers} />;
}
