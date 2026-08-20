# @amicu/auth

Empty placeholder. Do **not** implement here yet.

Per RFC 0001 §6–§7 and decision §11.6, this package will hold JWKS verification
and session helpers: verify the short-lived access JWT offline against the
issuer's `/.well-known/jwks.json`, read `tier`/`roles` claims for UX gating
(never for enforcement), and manage the httpOnly refresh cookie held by the BFF.

It consumes the auth issuer from **andypalmi/amicu-planning#78**;
implementation lands with that issue, not here.
