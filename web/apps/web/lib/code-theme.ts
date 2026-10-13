// One Shiki theme (light + dark) shared by the docs code blocks (fumadocs
// rehypeCode, see source.config.ts) and the marketing code panels.
//
// Mapping, identical for HCL, shell, JSON and TOML:
//   orange (accent-text)  keywords, HCL block types, function / command names,
//                         property and attribute keys
//   foreground            strings, numbers, constants, identifiers
//   muted-foreground      comments (italic), punctuation, operators
//
// Hex values are the sRGB equivalents of the design tokens in
// packages/ui/src/styles/tokens.css (Shiki cannot resolve CSS variables when
// it emits the paired --shiki-light / --shiki-dark values). All pairs are AA
// against the card and muted fills.
const palettes = {
  light: { fg: '#141412', muted: '#5c5b58', accent: '#b53900' },
  dark: { fg: '#efeeec', muted: '#a3a19e', accent: '#fa8746' },
} as const;

function build(mode: 'light' | 'dark') {
  const { fg, muted, accent } = palettes[mode];
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
