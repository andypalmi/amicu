import { providers } from '@/app/(marketing)/_lib/content';
import { ProviderList } from './provider-list';

/** Server wrapper so MDX can render the registry list without props. */
export function ProviderGrid() {
  return <ProviderList items={providers} />;
}
