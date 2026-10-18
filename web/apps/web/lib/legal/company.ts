// Single source of truth for legal/company identity.
//
// The privacy and terms pages are written in code (see app/(marketing)/privacy
// and /terms) and render from this config. They go live once their effective
// date and contact address are set. The imprint stays hidden (404, unlinked, not
// in the sitemap) until the identity fields (legalName, address, country) are
// filled, so no personal details are published while there is no company.
//
// OWNER TODO: when a company or VAT number exists, fill the identity fields (the
// imprint then appears) and set controllerName. Have counsel review the wording.

export interface CompanyConfig {
  /**
   * Name shown for the data controller in the privacy policy. Unset: the policy
   * says "the amicu maintainer". Do not set it to a personal name unless the
   * owner has chosen to publish it.
   */
  controllerName: string | null;
  /** Registered legal entity name (imprint only). */
  legalName: string | null;
  /** Postal address, lines separated by "\n" (imprint only). */
  address: string | null;
  country: string | null;
  /** Commercial register / company number. */
  registrationId: string | null;
  vatId: string | null;
  /** General public contact address. */
  contactEmail: string | null;
  /** Address for privacy and data requests. */
  privacyEmail: string | null;
  /** ISO dates (YYYY-MM-DD). A page is live only when its date is set. */
  privacyEffectiveDate: string | null;
  termsEffectiveDate: string | null;
}

export const company: CompanyConfig = {
  controllerName: null,
  legalName: null,
  address: null,
  country: null,
  registrationId: null,
  vatId: null,
  contactEmail: 'hello@amicu.dev',
  privacyEmail: 'privacy@amicu.dev',
  privacyEffectiveDate: '2026-10-18',
  termsEffectiveDate: '2026-10-15',
};

const identityReady = (c: CompanyConfig) =>
  Boolean(c.legalName && c.address && c.country && c.contactEmail);

export function resolveLegalPages(c: CompanyConfig) {
  return {
    privacy: Boolean(c.privacyEmail && c.privacyEffectiveDate),
    terms: Boolean(c.contactEmail && c.termsEffectiveDate),
    imprint: identityReady(c),
    contact: Boolean(c.contactEmail),
  };
}

export const legalPages = resolveLegalPages(company);
