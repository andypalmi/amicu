// Single source of truth for legal/company identity. ALL FIELDS ARE NULL BY
// DEFAULT, and while they are null no legal page, footer legal link, signup
// consent line or sitemap entry is emitted (the routes call notFound()).
//
// OWNER TODO: fill these in (and supply the policy bodies) before launch. Do not
// invent text: have counsel review privacy/terms wording.

export interface CompanyConfig {
  /** Registered legal entity name. */
  legalName: string | null;
  /** Postal address, lines separated by "\n". */
  address: string | null;
  country: string | null;
  /** Commercial register / company number. */
  registrationId: string | null;
  vatId: string | null;
  /** General public contact address. */
  contactEmail: string | null;
  /** Address for privacy and data requests. */
  privacyEmail: string | null;
  /** ISO dates (YYYY-MM-DD). */
  privacyEffectiveDate: string | null;
  termsEffectiveDate: string | null;
  /** Policy text supplied by the owner/counsel; paragraphs separated by a blank line. */
  privacyBody: string | null;
  termsBody: string | null;
}

export const company: CompanyConfig = {
  legalName: null,
  address: null,
  country: null,
  registrationId: null,
  vatId: null,
  contactEmail: 'hello@amicu.dev',
  privacyEmail: 'privacy@amicu.dev',
  privacyEffectiveDate: null,
  termsEffectiveDate: null,
  privacyBody: null,
  termsBody: null,
};

const identityReady = (c: CompanyConfig) =>
  Boolean(c.legalName && c.address && c.country && c.contactEmail);

export function resolveLegalPages(c: CompanyConfig) {
  const id = identityReady(c);
  return {
    privacy: id && Boolean(c.privacyEffectiveDate && c.privacyBody),
    terms: id && Boolean(c.termsEffectiveDate && c.termsBody),
    imprint: id,
    contact: Boolean(c.contactEmail),
  };
}

export const legalPages = resolveLegalPages(company);
