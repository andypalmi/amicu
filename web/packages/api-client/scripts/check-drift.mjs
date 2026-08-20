// CI drift guard for the generated OpenAPI client.
//
// Regenerates `src/schema.ts` from the committed spec and fails if the result
// differs from what is checked in. This makes the committed client provably a pure
// function of `crates/amicu-schema/openapi.json`: if the API changes the spec but
// nobody reruns `pnpm generate`, CI goes red.
//
// Usage: `pnpm --filter @amicu/api-client check:generated`

import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';

const pkgDir = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const schema = 'src/schema.ts';

function run(cmd, args) {
  return execFileSync(cmd, args, { cwd: pkgDir, encoding: 'utf8' });
}

// Regenerate in place using the pinned openapi-typescript from the package script.
run('pnpm', ['run', 'generate']);

// `git status --porcelain` reports the file as dirty whether it was *modified*
// (regeneration changed committed bytes) or *untracked* (the generated file was
// never committed at all) — `git diff` alone would silently miss the untracked
// case. Scoped to the generated file so unrelated edits never trip (or mask) it.
const status = run('git', ['status', '--porcelain', '--', schema]).trim();
if (status === '') {
  console.log(`✓ ${schema} is up to date with crates/amicu-schema/openapi.json`);
} else {
  // Show what changed for the CI log.
  process.stdout.write(run('git', ['diff', '--', schema]));
  console.error(
    `\n✗ ${schema} is out of date with (or not committed against) ` +
      'crates/amicu-schema/openapi.json.\n' +
      '  Regenerate it and commit the result:\n\n' +
      '      pnpm --filter @amicu/api-client generate\n',
  );
  process.exit(1);
}
