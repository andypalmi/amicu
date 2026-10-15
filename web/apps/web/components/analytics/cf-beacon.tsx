import Script from 'next/script';
import { CF_BEACON_SRC, cfBeaconToken } from '@/lib/analytics/config';

/** Cloudflare Web Analytics (cookieless). Renders nothing unless the token is set at build time. */
export function CloudflareBeacon() {
  const token = cfBeaconToken(process.env.NEXT_PUBLIC_CF_BEACON_TOKEN);
  if (!token) return null;
  return (
    <Script
      defer
      src={CF_BEACON_SRC}
      data-cf-beacon={JSON.stringify({ token })}
      strategy="afterInteractive"
    />
  );
}
