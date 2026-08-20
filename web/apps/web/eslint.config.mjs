import next from '@amicu/config/eslint/next';

const config = [
  ...next,
  {
    ignores: ['.next/**', '.source/**', 'next-env.d.ts'],
  },
];

export default config;
