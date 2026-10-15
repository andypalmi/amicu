// Sign-up consent: a required checkbox confirming the user is 18 or older and
// accepts the Terms and Privacy Policy. Shared by the client form and the server
// action so both validate the same field.

export const ACCEPT_TERMS_FIELD = 'acceptTerms';
export const ACCEPT_TERMS_ERROR =
  'You must confirm you are 18 or older and accept the Terms and Privacy Policy.';

/** True when the submitted checkbox value means "checked". */
export function termsAccepted(value: FormDataEntryValue | null): boolean {
  return value === 'on';
}
