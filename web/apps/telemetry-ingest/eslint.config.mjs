import base from '@amicu/config/eslint/base';

export default [...base, { ignores: ['.wrangler/**', 'dist/**'] }];
