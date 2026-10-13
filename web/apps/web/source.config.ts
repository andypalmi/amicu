import { defineConfig } from 'fumadocs-mdx/config';
import { codeThemes } from './lib/code-theme';

export default defineConfig({
  mdxOptions: {
    rehypeCodeOptions: {
      themes: codeThemes,
    },
  },
});
