// Flat ESLint config for the Next.js app: base + Next core-web-vitals.
import next from 'eslint-config-next';
import base from './base.mjs';

const nextConfigs = Array.isArray(next) ? next : [next];

/** @type {import('eslint').Linter.Config[]} */
export default [...base, ...nextConfigs];
