// Client-safe input validation for the auth forms. Shared by the client forms
// (fast feedback) and re-checked in the server actions before any network call.
// zod is isomorphic, so these run in both the browser and the BFF.

import { z } from 'zod';

const email = z.string().trim().min(1, 'Email is required.').email('Enter a valid email address.');

// Deliberately loose on the *format* of an existing password at login (the server
// is the authority). At signup we enforce a minimum length for basic hygiene.
const password = z.string().min(1, 'Password is required.');
const newPassword = z
  .string()
  .min(8, 'Use at least 8 characters.')
  .max(256, 'That password is too long.');

export const loginSchema = z.object({
  email,
  password,
});

export const signupSchema = z.object({
  email,
  password: newPassword,
});

export const forgotSchema = z.object({
  email,
});

export type LoginInput = z.infer<typeof loginSchema>;
export type SignupInput = z.infer<typeof signupSchema>;
export type ForgotInput = z.infer<typeof forgotSchema>;

/** Field-keyed validation errors, ready to render under each input. */
export type FieldErrors = Record<string, string>;

/** Flatten a zod error into a { field: firstMessage } map. */
export function fieldErrors(error: z.ZodError): FieldErrors {
  const out: FieldErrors = {};
  for (const issue of error.issues) {
    const key = issue.path[0];
    if (typeof key === 'string' && !(key in out)) {
      out[key] = issue.message;
    }
  }
  return out;
}
