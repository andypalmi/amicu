// Declarative timeline + pure reducer for the home apply-to-online demo.
// `demoState(steps, t)` maps elapsed ms to what is visible, so the component
// is a thin clock and the behaviour is unit-testable without a DOM.
//
// Real formats (crates/amicu-cli/src/commands): `Plan: N action(s).` and the
// `+ address` rows (plan.rs), `Apply these changes? [y/N]` (apply.rs:842),
// `Applied N deployment(s):` and `  <id> -> <endpoint>` (apply.rs:710-712).
// The CLI prints no per-resource spinner and amicu has no chat TUI, so the
// spinner rows and the chat frame are illustrative.

export type Tone = 'cmd' | 'add' | 'muted' | 'plain';
export type Screen = 'term' | 'chat';
export type Status = 'starting' | 'online';

interface Base {
  at: number;
  screen: Screen;
  tone?: Tone;
}
export type Step =
  | (Base & { kind: 'type'; text: string; msPerChar?: number })
  | (Base & { kind: 'line'; text: string })
  | (Base & { kind: 'spinner'; text: string; until: number })
  | (Base & { kind: 'stream'; text: string; msPerToken?: number })
  | { kind: 'status'; at: number; status: Status }
  | { kind: 'screen'; at: number; screen: Screen };

export interface Row {
  visible: boolean;
  screen: Screen;
  tone: Tone;
  /** Leading mark: spinner frame, check, or empty. */
  mark: string;
  /** Reserve the mark column (spinner rows). */
  hasMark: boolean;
  shown: string;
  /** Not yet revealed; rendered invisible so wrapping never shifts. */
  rest: string;
}
export interface DemoState {
  screen: Screen;
  status: Status | null;
  rows: Row[];
  done: boolean;
}

const FRAMES = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
export const URL_LINE =
  '  deployment.chat -> https://<workspace>--amicu-chat-server.<region>.modal.direct';
export const PROMPT = 'Draft a retention notice for our EU customers.';
const REPLY =
  'We keep customer data only in EU regions and delete it 30 days after you close your account. You can ask for earlier deletion at any time.';

export const DEMO_STEPS: Step[] = [
  { kind: 'type', at: 300, screen: 'term', tone: 'cmd', text: '$ amicu apply' },
  { kind: 'status', at: 1100, status: 'starting' },
  { kind: 'line', at: 1100, screen: 'term', text: 'Plan: 2 action(s).' },
  { kind: 'line', at: 1250, screen: 'term', tone: 'add', text: '  + hardware_profile.gpu' },
  { kind: 'line', at: 1400, screen: 'term', tone: 'add', text: '  + deployment.chat' },
  { kind: 'line', at: 1700, screen: 'term', text: 'Apply these changes? [y/N] y' },
  { kind: 'spinner', at: 2000, until: 3200, screen: 'term', text: 'hardware_profile.gpu  modal eu-west gpu.l4' },
  { kind: 'spinner', at: 3200, until: 5000, screen: 'term', text: 'deployment.chat  vllm' },
  { kind: 'line', at: 5200, screen: 'term', text: 'Applied 1 deployment(s):' },
  { kind: 'line', at: 5400, screen: 'term', tone: 'muted', text: URL_LINE },
  { kind: 'status', at: 5400, status: 'online' },
  { kind: 'screen', at: 6800, screen: 'chat' },
  { kind: 'type', at: 7200, screen: 'chat', text: PROMPT },
  { kind: 'stream', at: 9000, screen: 'chat', text: REPLY },
];

function tokens(text: string): string[] {
  return text.match(/\S+\s*/g) ?? [];
}

/** Time at which every step has finished. */
export function demoDuration(steps: Step[]): number {
  let end = 0;
  for (const s of steps) {
    let e = s.at;
    if (s.kind === 'type') e += s.text.length * (s.msPerChar ?? 35);
    else if (s.kind === 'spinner') e = s.until;
    else if (s.kind === 'stream') e += tokens(s.text).length * (s.msPerToken ?? 55);
    end = Math.max(end, e);
  }
  return end;
}

export function demoState(steps: Step[], t: number): DemoState {
  let screen: Screen = 'term';
  let status: Status | null = null;
  const rows: Row[] = [];
  for (const s of steps) {
    const started = t >= s.at;
    if (s.kind === 'screen') {
      if (started) screen = s.screen;
      continue;
    }
    if (s.kind === 'status') {
      if (started) status = s.status;
      continue;
    }
    let mark = '';
    let shown = '';
    if (s.kind === 'type') {
      shown = s.text.slice(0, Math.max(0, Math.floor((t - s.at) / (s.msPerChar ?? 35))));
    } else if (s.kind === 'stream') {
      const n = Math.floor((t - s.at) / (s.msPerToken ?? 55)) + 1;
      shown = started ? tokens(s.text).slice(0, n).join('') : '';
    } else {
      shown = started ? s.text : '';
      if (s.kind === 'spinner' && started) {
        mark = t >= s.until ? '✓' : FRAMES[Math.floor((t - s.at) / 80) % FRAMES.length];
      }
    }
    rows.push({
      visible: started,
      screen: s.screen,
      tone: s.tone ?? 'plain',
      mark,
      hasMark: s.kind === 'spinner',
      shown,
      rest: s.text.slice(shown.length),
    });
  }
  return { screen, status, rows, done: t >= demoDuration(steps) };
}
