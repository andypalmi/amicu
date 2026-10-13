import assert from 'node:assert/strict';
import { test } from 'node:test';
import { findColors } from './check-colors-lib.mjs';

const hits = (text, file = 'x.tsx') => findColors(text, file).map((f) => f.text);
const flagged = (text, file) => assert.ok(hits(text, file).length > 0, `expected a finding in: ${text}`);
const clean = (text, file) => assert.deepEqual(hits(text, file), [], `expected no finding in: ${text}`);

test('hex in all lengths and contexts', () => {
  for (const h of ['#fff', '#ffff', '#ffffff', '#ffffffaa', '#FFF', '#0a0a0a', '#000', '#123456']) {
    flagged(`const c = "${h}";`);
    flagged(`.a { color: ${h}; }`, 'a.css');
    flagged(`<rect fill="${h}" />`, 'a.svg');
    flagged(`<div style={{ color: '${h}' }} />`);
    flagged(`{"foreground": "${h}"}`, 'theme.json');
  }
  flagged('background: linear-gradient(#fff, #000);', 'a.css');
  flagged('className="bg-[#fff] text-[#ffffff80]"');
  flagged('className="shadow-[0_0_0_1px_#000]"');
});

test('hex false positives', () => {
  clean('Fixes #191 and see (#217) or Part of andypalmi/amicu-planning#201');
  clean('See issue #123456 for details', 'a.md');
  clean('<a href="#add">x</a> <a href="/docs#fade">y</a> [t](#dead)');
  clean('&#39; &#x27; &#123;');
  clean('commit 7aa0d7f and sha deadbeef1234');
  clean('url(#abc) fill="url(#fade)"');
  clean('#add { margin: 0 }', 'a.css');
  clean('a, #fade > b { margin: 0 }', 'a.css');
  clean('# Heading\n## Another');
  clean('const id = "x#12345";');
  clean('const n = 0xfff; const t = `${a}#${b}`;');
});

test('colour functions with literal channels', () => {
  for (const f of [
    'rgb(0 0 0)', 'rgba(0,0,0,.5)', 'hsl(210 10% 50%)', 'hsla(210, 10%, 50%, .5)', 'hwb(210 10% 10%)',
    'lab(50% 10 10)', 'lch(50% 30 40)', 'oklab(0.5 0.1 0.1)', 'oklch(0.5 0.1 40)', 'oklch(0.5 0.1 40 / 0.3)',
    'color(display-p3 1 0 0)', 'color(srgb 0.1 0.2 0.3)', 'color-mix(in srgb, red, blue)',
    'color-mix(in oklab, #fff 10%, var(--a))', 'color-mix(in oklab, var(--a), black 10%)',
    'color-mix(in oklab, oklch(0.5 0.1 40), transparent)',
  ]) {
    flagged(`.a { background: ${f}; }`, 'a.css');
    flagged(`const c = '${f}';`);
  }
  flagged('className="bg-[rgb(0,0,0)] hover:bg-[oklch(0.5_0.1_40)]"');
});

test('colour function false positives', () => {
  clean('.a { color: rgb(var(--x)); }', 'a.css');
  clean('.a { color: oklch(from var(--primary) l c h / 0.5); }', 'a.css');
  clean('.a { background: color-mix(in oklab, var(--diff-destroy) 14%, transparent); }', 'a.css');
  clean('.a { background: color-mix(in oklab, var(--a) 40%, currentColor); }', 'a.css');
  clean('const x = color(value); function lab(x) {}; lch(a, b);');
});

test('named colours in CSS, style objects and SVG', () => {
  flagged('.a { color: white; }', 'a.css');
  flagged('.a { background: black; }', 'a.css');
  flagged('.a { border: 1px solid red; }', 'a.css');
  flagged('.a { box-shadow: 0 0 4px gray; }', 'a.css');
  flagged('.a { fill: Gray }', 'a.css');
  flagged('.a { background: linear-gradient(to right, white, var(--x)); }', 'a.css');
  flagged('<div style={{ color: "white" }} />');
  flagged("<div style={{ backgroundColor: 'rebeccapurple' }} />");
  flagged('<div style="border-color: tomato" />');
  flagged('<path fill="red" stroke="black" />');
  flagged('<stop stop-color="white" stopColor="white" />', 'a.svg');
  flagged("ctx.fillStyle = 'red';");
  flagged("g.addColorStop(0, 'blue');");
  flagged("const theme = { foreground: 'white', bg: 'black' };");
  flagged("const chart = { color: 'steelblue' };");
  flagged('{"background": "navy"}', 'theme.json');
  flagged("const style = { boxShadow: '0 0 0 1px silver' };");
});

test('named colour false positives', () => {
  clean('.a { color: var(--foreground); background: transparent; border: none; }', 'a.css');
  clean('.a { color: currentColor; fill: currentColor; stroke: none; }', 'a.css');
  clean('.a { color: inherit; background: initial; border-color: unset; }', 'a.css');
  clean('.a { border: 1px solid var(--border); }', 'a.css');
  clean('<path fill="currentColor" stroke="none" />');
  clean('<div style={{ color: "var(--accent-text)" }} />');
  clean('.a { background: url(white.png) var(--x); }', 'a.css');
  clean('const white = 1; const red = "x";');
  clean('interface P { color: string; fill?: boolean }');
  clean('The tan box is red in some places, white elsewhere.', 'a.md');
});

test('Tailwind palette classes with variants, opacity and important', () => {
  for (const c of [
    'text-red-500', 'bg-white', 'bg-black', 'border-zinc-200', 'from-blue-500', 'to-emerald-600', 'via-slate-900',
    'ring-red-500', 'fill-white', 'stroke-black', 'decoration-pink-300', 'outline-amber-400', 'shadow-black',
    'accent-lime-500', 'caret-sky-500', 'divide-gray-200', 'placeholder-neutral-400', 'border-t-red-500',
    'ring-offset-white', 'bg-white/50', 'text-red-500/75', '!bg-white', 'bg-white!', 'dark:bg-black',
    'hover:text-red-500', 'md:dark:hover:bg-zinc-900', 'group-hover:text-white', 'data-[state=open]:bg-white',
    '[&_pre]:bg-black', 'has-[:checked]:border-blue-500', 'dark:hover:!bg-white/10', 'shadow-white/5',
  ]) {
    flagged(`<div className="p-2 ${c} m-1" />`);
    flagged(`<div className={cn('x', "${c}")} />`);
  }
  flagged('.a { @apply bg-white text-red-500; }', 'a.css');
  flagged('.a { color: theme(colors.red.500); }', 'a.css');
  flagged('.a { color: theme(--color-blue-500); }', 'a.css');
  flagged('.a { color: var(--color-red-500); }', 'a.css');
});

test('Tailwind arbitrary values', () => {
  flagged('className="text-[#fff]"');
  flagged('className="bg-[rgb(0_0_0)]"');
  flagged('className="bg-[red]"');
  flagged('className="[color:red]"');
  flagged('className="[background:white]"');
  flagged('className="[--x:white]"');
  flagged('className="shadow-[0_0_0_1px_red]"');
  flagged('className="shadow-[0_1px_2px_#000]"');
  flagged('className="dark:hover:border-[tomato]"');
});

test('Tailwind tokens and neutral values are allowed', () => {
  for (const c of [
    'text-foreground', 'bg-background', 'bg-card', 'border-border', 'text-muted-foreground', 'bg-primary',
    'hover:bg-primary-hover', 'text-accent-text', 'ring-ring', 'bg-overlay', 'bg-transparent', 'text-current',
    'text-inherit', 'border-none', 'bg-primary/50', 'dark:bg-secondary', 'fill-current', 'stroke-current',
    'text-[color:var(--x)]', 'bg-(--brand)', 'text-sm', 'text-center', 'shadow-lg', 'border-2', 'ring-2',
    'outline-none', 'bg-[var(--card)]', 'from-primary', 'to-transparent', 'divide-y', 'text-balance',
    'border-t', 'bg-gradient-to-r', 'bg-[color-mix(in_oklab,var(--primary),var(--foreground)_10%)]',
  ]) {
    clean(`<div className="p-2 ${c} m-1" />`);
  }
  clean('.a { color: var(--foreground); background: var(--card); }', 'a.css');
});

test('JS colour constants passed to libraries', () => {
  flagged("const t = { name: 'x', fg: '#141412', bg: 'transparent' };");
  flagged("new ImageResponse(<div style={{ background: '#0a0a0a', color: '#fafafa' }} />)");
  flagged("const config = { series: { color: 'hsl(210 10% 50%)' } };");
  flagged("themes: { light: { foreground: 'rgb(20, 20, 18)' } }");
  clean("const t = { fg: 'var(--foreground)', bg: 'transparent' };");
});

test('reports line and column', () => {
  const [f] = findColors('a\n  color: "#fff"\n', 'x.ts');
  assert.equal(f.line, 2);
  assert.equal(f.col, 11);
  assert.equal(f.text, '#fff');
});
