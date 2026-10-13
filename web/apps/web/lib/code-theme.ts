// One Shiki theme shared by the docs code blocks (fumadocs rehypeCode, see
// source.config.ts) and the marketing code panels.
//
// Colours are CSS variables from packages/ui/src/styles/tokens.css, so light
// and dark both come from the tokens (they are redefined under `.dark`) and no
// colour literal lives here. Shiki emits them as `--shiki-light` /
// `--shiki-dark` custom properties which resolve to the same live tokens.
//
// Mapping, identical for HCL, shell, JSON and TOML:
//   orange (accent-text)  keywords, HCL block types, function / command names,
//                         property and attribute keys
//   foreground            strings, numbers, constants, identifiers
//   muted-foreground      comments (italic), punctuation, operators
const fg = 'var(--foreground)';
const muted = 'var(--muted-foreground)';
const accent = 'var(--accent-text)';

function build(mode: 'light' | 'dark') {
  return {
    name: `amicu-${mode}`,
    type: mode,
    fg,
    bg: 'transparent',
    settings: [
      { settings: { foreground: fg } },
      {
        scope: [
          'keyword',
          'storage',
          'storage.type',
          'keyword.control',
          'support.function',
          'entity.name.function',
          'entity.name.command',
          'support.function.builtin.shell',
        ],
        settings: { foreground: accent },
      },
      { scope: ['entity.name.type', 'entity.name.tag'], settings: { foreground: accent, fontStyle: 'bold' } },
      {
        scope: [
          'variable.other.readwrite',
          'support.type.property-name',
          'meta.object-literal.key',
          'entity.name.tag.toml',
          'support.type.property-name.toml',
        ],
        settings: { foreground: accent },
      },
      {
        scope: [
          'string',
          'constant',
          'constant.numeric',
          'variable.other.enummember',
          'variable.other.member',
          'variable.parameter',
        ],
        settings: { foreground: fg },
      },
      { scope: ['keyword.operator', 'punctuation', 'meta.brace'], settings: { foreground: muted } },
      { scope: ['comment', 'punctuation.definition.comment'], settings: { foreground: muted, fontStyle: 'italic' } },
    ],
  };
}

export const codeThemes = { light: build('light'), dark: build('dark') };
