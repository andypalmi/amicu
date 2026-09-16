import next from '@amicu/config/eslint/next';

// Messages shared by the browser-dialog guard below.
const NO_BROWSER_DIALOG =
  'Do not use browser alert/confirm/prompt. For transient feedback use `toast` ' +
  'from lib/notify/toast.ts; for persistent in-context errors use the ErrorAlert ' +
  'component (components/common/error-alert.tsx).';

const config = [
  ...next,
  {
    // .open-next/ and .wrangler/ are OpenNext/Wrangler's generated Cloudflare
    // Workers build output (bundled/minified server code, dev-server state) -
    // gitignored (web/.gitignore) and not meant to be linted, same as .next/.
    ignores: [
      '.next/**',
      '.source/**',
      'next-env.d.ts',
      '.open-next/**',
      '.wrangler/**',
      'cloudflare-env.d.ts',
    ],
  },
  {
    // Hard guard: fail `lint` (and therefore CI's web job) on any use of the
    // blocking browser dialogs. `no-restricted-globals` resolves references to
    // the real global, so it flags `alert(...)` / `confirm(...)` / `prompt(...)`
    // call sites but NOT innocent identifiers like `confirmPassword`,
    // `alertTitle`, or a locally-declared `const confirm = …` (which shadows the
    // global). `no-restricted-properties` covers the `window.alert` / `.confirm`
    // / `.prompt` member forms.
    rules: {
      'no-restricted-globals': [
        'error',
        { name: 'alert', message: NO_BROWSER_DIALOG },
        { name: 'confirm', message: NO_BROWSER_DIALOG },
        { name: 'prompt', message: NO_BROWSER_DIALOG },
      ],
      'no-restricted-properties': [
        'error',
        { object: 'window', property: 'alert', message: NO_BROWSER_DIALOG },
        { object: 'window', property: 'confirm', message: NO_BROWSER_DIALOG },
        { object: 'window', property: 'prompt', message: NO_BROWSER_DIALOG },
      ],
    },
  },
];

export default config;
