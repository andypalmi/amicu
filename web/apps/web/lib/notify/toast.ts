// Sanctioned entry point for TRANSIENT notifications.
//
// Toasts are ephemeral, non-blocking feedback that appears and then auto-
// dismisses - confirmations and non-blocking errors the user does not have to
// act on in place:
//
//   toast.success('Plan applied');
//   toast('Saved');
//   toast.error('Copy failed - try again');
//
// For PERSISTENT, in-context errors that must stay on screen until the user
// resolves them (a failed form section, a failed console request), use the
// `<ErrorAlert>` component in `components/common/error-alert.tsx` - built on the
// `@amicu/ui` `<Alert>` - NOT a toast.
//
// Import `toast` from here, never from `sonner` directly, so there is a single
// choke point for transient notifications (styling, telemetry, or swapping the
// underlying library later). A lint guard keeps browser `alert/confirm/prompt`
// out of the codebase entirely - see `eslint.config.mjs`.
import { toast } from 'sonner';

export { toast };
