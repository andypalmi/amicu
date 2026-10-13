// Token-only colour guard: detection logic. See check-colors.mjs (CLI) and
// check-colors.test.mjs. Colours may only come from the CSS tokens in
// packages/ui/src/styles/tokens.css (var(--token) or Tailwind classes mapped
// to them). Everything else that spells a colour is a finding.

export const NAMED_COLORS = new Set(
  (
    'aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue blueviolet brown ' +
    'burlywood cadetblue chartreuse chocolate coral cornflowerblue cornsilk crimson cyan darkblue darkcyan ' +
    'darkgoldenrod darkgray darkgreen darkgrey darkkhaki darkmagenta darkolivegreen darkorange darkorchid ' +
    'darkred darksalmon darkseagreen darkslateblue darkslategray darkslategrey darkturquoise darkviolet ' +
    'deeppink deepskyblue dimgray dimgrey dodgerblue firebrick floralwhite forestgreen fuchsia gainsboro ' +
    'ghostwhite gold goldenrod gray green greenyellow grey honeydew hotpink indianred indigo ivory khaki ' +
    'lavender lavenderblush lawngreen lemonchiffon lightblue lightcoral lightcyan lightgoldenrodyellow ' +
    'lightgray lightgreen lightgrey lightpink lightsalmon lightseagreen lightskyblue lightslategray ' +
    'lightslategrey lightsteelblue lightyellow lime limegreen linen magenta maroon mediumaquamarine ' +
    'mediumblue mediumorchid mediumpurple mediumseagreen mediumslateblue mediumspringgreen mediumturquoise ' +
    'mediumvioletred midnightblue mintcream mistyrose moccasin navajowhite navy oldlace olive olivedrab ' +
    'orange orangered orchid palegoldenrod palegreen paleturquoise palevioletred papayawhip peachpuff peru ' +
    'pink plum powderblue purple rebeccapurple red rosybrown royalblue saddlebrown salmon sandybrown ' +
    'seagreen seashell sienna silver skyblue slateblue slategray slategrey snow springgreen steelblue tan ' +
    'teal thistle tomato turquoise violet wheat white whitesmoke yellow yellowgreen'
  ).split(' '),
);

const PALETTE =
  'slate|gray|zinc|neutral|stone|red|orange|amber|yellow|lime|green|emerald|teal|cyan|sky|blue|indigo|violet|purple|fuchsia|pink|rose|taupe|mauve|mist|olive';
const SHADE = '50|100|200|300|400|500|600|700|800|900|950';
const TW_PREFIX =
  'text|bg|border(?:-[xytrblse])?|from|via|to|ring-offset|ring|inset-ring|inset-shadow|drop-shadow|fill|stroke|decoration|outline|shadow|accent|caret|divide|placeholder';
const COLOR_PROPS =
  'color|background(?:-color)?|border(?:-(?:top|right|bottom|left|block|inline)(?:-(?:start|end))?)?(?:-color)?|outline(?:-color)?|fill|stroke|stop-color|flood-color|lighting-color|caret-color|accent-color|text-decoration(?:-color)?|text-shadow|box-shadow|column-rule(?:-color)?|scrollbar-color|-webkit-text-fill-color|-webkit-text-stroke(?:-color)?|text-emphasis-color|filter';
const JS_KEYS =
  '\\w*[Cc]olou?r|fill|stroke|background|foreground|fg|bg|border|outline|boxShadow|textShadow|fillStyle|strokeStyle|filter|stop-color|flood-color|lighting-color';

/** Blank out var()/url()/theme() so token references never count as literals. */
function stripSafe(s) {
  let prev;
  do {
    prev = s;
    s = s.replace(/(?:var|url|theme|env)\([^()]*\)/gi, (m) => ' '.repeat(m.length));
  } while (s !== prev);
  return s;
}

/** Named colours inside a value string, with offsets. */
function namedHits(value) {
  const v = stripSafe(value);
  const out = [];
  for (const m of v.matchAll(/(?<![\w-])[A-Za-z]+(?![\w-])/g)) {
    if (NAMED_COLORS.has(m[0].toLowerCase())) out.push({ index: m.index, text: m[0] });
  }
  return out;
}

/** Content of the balanced paren group whose "(" is at `open`. */
function balanced(text, open) {
  let depth = 0;
  for (let i = open; i < text.length && i - open < 600; i++) {
    if (text[i] === '(') depth++;
    else if (text[i] === ')' && --depth === 0) return text.slice(open + 1, i);
  }
  return text.slice(open + 1, open + 200);
}

const LITERAL_FN = /(?<![\w-])(rgba?|hsla?|hwb|lab|lch|oklab|oklch|color-mix|color)\(/gi;
const COLOR_SPACES = /^(srgb|srgb-linear|display-p3|a98-rgb|prophoto-rgb|rec2020|xyz|xyz-d50|xyz-d65)\b/i;

function fnIsLiteral(name, args) {
  const a = stripSafe(args).trim();
  const n = name.toLowerCase();
  if (n === 'color') return COLOR_SPACES.test(a) && /\d/.test(a);
  if (n === 'color-mix') {
    return (
      /#[0-9a-f]{3,8}\b/i.test(a) ||
      namedHits(a).length > 0 ||
      /(?<![\w-])(rgba?|hsla?|hwb|lab|lch|oklab|oklch)\(\s*[\d.+\-%]/i.test(a)
    );
  }
  // Relative colour syntax: only literal when the origin colour is.
  if (/^from\s/i.test(a)) return fnIsLiteral('color-mix', a.replace(/^from\s+/i, ''));
  return /^[\d.+\-%]/.test(a) || /^none\b/i.test(a);
}

const ANCHOR_PREFIX =
  /(?:href|src|action|to|id|for|xlink:href|data-[\w-]+|aria-[\w-]+|url\(|\]\()\s*[=:(]?\s*\{?["'`]?$/i;

export function findColors(text, filename = 'x.ts') {
  const ext = (filename.match(/\.([a-z0-9]+)$/i)?.[1] ?? '').toLowerCase();
  const isCss = ext === 'css';
  const isProse = ext === 'md' || ext === 'mdx';
  const hits = [];
  const add = (index, str, rule) => hits.push({ index, text: str, rule });

  // 1. Hex (#rgb #rgba #rrggbb #rrggbbaa)
  for (const m of text.matchAll(/(?<![A-Za-z0-9&/.\-])#([0-9a-fA-F]{3,8})(?![\w-])/g)) {
    const digits = m[1];
    if (![3, 4, 6, 8].includes(digits.length)) continue;
    const before = text.slice(Math.max(0, m.index - 60), m.index);
    const lineBefore = before.slice(before.lastIndexOf('\n') + 1);
    if (ANCHOR_PREFIX.test(lineBefore)) continue;
    const prev = text[m.index - 1] ?? '';
    if (/^\d+$/.test(digits)) {
      // Issue refs ("#191") look like all-digit hex: need an unambiguous colour context.
      const quoted = /["'`[_]/.test(prev);
      const cssValue = isCss && /[\w-]\s*:\s*$|[(,]\s*$/.test(lineBefore);
      if (!quoted && !cssValue) continue;
      if (isProse && !/["'`]/.test(prev)) continue;
    }
    if (isCss) {
      // `#add { ... }` is an id selector, not a colour.
      const after = text.slice(m.index + m[0].length);
      const next = after.search(/[{;}]/);
      if (next !== -1 && after[next] === '{') continue;
    }
    add(m.index, m[0], 'hex');
  }

  // 2. Colour functions with literal channels
  for (const m of text.matchAll(LITERAL_FN)) {
    const open = m.index + m[0].length - 1;
    const args = balanced(text, open);
    if (fnIsLiteral(m[1], args)) {
      add(m.index, `${m[1]}(${args.length > 40 ? args.slice(0, 40) + '...' : args})`, 'function');
    }
  }

  // 3a. Named colours in CSS declarations (CSS, SVG <style>, style="" strings)
  const decl = new RegExp(`(?<![\\w-])(?:${COLOR_PROPS})\\s*:\\s*([^;{}"'\`\\n]+)`, 'gi');
  for (const m of text.matchAll(decl)) {
    const vstart = m.index + m[0].length - m[1].length;
    for (const h of namedHits(m[1])) add(vstart + h.index, h.text, 'named');
  }
  // 3b. Named colours in JS style objects, JSX/SVG attributes, canvas, chart/theme configs
  const js = new RegExp(`(?<![\\w-])["']?(?:${JS_KEYS})["']?\\s*[:=]\\s*\\{?\\s*(["'\`])([^"'\`\\n]*)\\1`, 'g');
  for (const m of text.matchAll(js)) {
    const vstart = m.index + m[0].indexOf(m[1]) + 1;
    for (const h of namedHits(m[2])) add(vstart + h.index, h.text, 'named');
  }
  for (const m of text.matchAll(/addColorStop\(\s*[^,()]+,\s*(["'`])([^"'`]*)\1/g)) {
    const vstart = m.index + m[0].indexOf(m[1]) + 1;
    for (const h of namedHits(m[2])) add(vstart + h.index, h.text, 'named');
  }

  // 4. Tailwind palette classes (any variant prefix, /opacity, ! modifier)
  const pal = new RegExp(
    `(?<=^|[\\s"'\`:!{(,>.])(?:${TW_PREFIX})-(?:(?:${PALETTE})-(?:${SHADE})|black|white)(?:\\/[\\w.%\\[\\]()-]+)?!?(?![\\w\\[-])`,
    'gm',
  );
  for (const m of text.matchAll(pal)) add(m.index, m[0], 'tailwind-palette');
  // Tailwind arbitrary colour values: bg-[red], shadow-[0_0_0_1px_red] (hex/functions caught above)
  const arb = new RegExp(`(?<=^|[\\s"'\`:!{(,>.])(?:${TW_PREFIX})-\\[([^\\]\\s]+)\\]`, 'gm');
  for (const m of text.matchAll(arb)) {
    const vstart = m.index + m[0].indexOf('[') + 1;
    for (const h of namedHits(m[1].replace(/_/g, ' '))) add(vstart + h.index, h.text, 'tailwind-arbitrary');
  }
  // Arbitrary properties: [color:red], [--x:white]
  const prop = new RegExp(`\\[(?:${COLOR_PROPS}|--[\\w-]+):([^\\]\\s]+)\\]`, 'g');
  for (const m of text.matchAll(prop)) {
    const vstart = m.index + m[0].indexOf(':') + 1;
    for (const h of namedHits(m[1].replace(/_/g, ' '))) add(vstart + h.index, h.text, 'tailwind-arbitrary');
  }
  // Palette via theme variables / theme()
  const themeRe = new RegExp(
    `(?<![\\w-])--color-(?:(?:${PALETTE})(?:-(?:${SHADE}))?|black|white)(?![\\w-])|theme\\(\\s*["']?colors\\.(?:(?:${PALETTE})|black|white)\\b[^)]*\\)`,
    'g',
  );
  for (const m of text.matchAll(themeRe)) add(m.index, m[0], 'theme-palette');

  const seen = new Set();
  return hits
    .filter((h) => (seen.has(h.index) ? false : (seen.add(h.index), true)))
    .sort((a, b) => a.index - b.index)
    .map((h) => ({ ...h, ...lineCol(text, h.index) }));
}

function lineCol(text, index) {
  let line = 1;
  let last = -1;
  for (let i = text.indexOf('\n'); i !== -1 && i < index; i = text.indexOf('\n', i + 1)) {
    line++;
    last = i;
  }
  return { line, col: index - last };
}
