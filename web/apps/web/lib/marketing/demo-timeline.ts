// Declarative timeline + pure reducer for the home apply-to-online demo.
// `demoState(steps, t)` maps elapsed ms to what is visible, so the component
// is a thin clock and the behaviour is unit-testable without a DOM.
//
// Every line is real CLI output or a real request: plan.rs (`Plan:` block),
// apply.rs:842 (`Apply these changes? [y/N]`), apply.rs:710-718 (`Applied`,
// `<id> -> <endpoint>`, then the `curl:` usage snippet from usage_snippets),
// and the OpenAI-compatible /v1/chat/completions that vLLM serves. Only time
// is compressed.

export type Tone = 'cmd' | 'add' | 'muted' | 'plain';
export type Screen = 'term' | 'call';
export type Status = 'starting' | 'online';

interface Base {
  at: number;
  screen: Screen;
  tone?: Tone;
}
export type Step =
  | (Base & { kind: 'type'; text: string; msPerChar?: number })
  | (Base & { kind: 'line'; text: string })
  | { kind: 'status'; at: number; status: Status }
  | { kind: 'screen'; at: number; screen: Screen };

export interface Row {
  screen: Screen;
  tone: Tone;
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

const URL = 'https://<workspace>--amicu-chat-server.<region>.modal.direct';
const MODEL = 'RedHatAI/Qwen3.5-9B-FP8-dynamic';
const BODY = `{"model": "${MODEL}", "messages": [{"role": "user", "content": "Hello"}]}`;
const CURL_ARGS = [
  `curl '${URL}/v1/chat/completions' \\`,
  '  -H "Authorization: Bearer $CHAT_API_KEY" \\',
  '  -H "Modal-Key: $MODAL_PROXY_TOKEN_ID" \\',
  '  -H "Modal-Secret: $MODAL_PROXY_TOKEN_SECRET" \\',
  '  -H "Content-Type: application/json" \\',
  `  -d '${BODY}'`,
];
// Non-streaming body, as vLLM returns it: the curl above sends no `stream`.
const RESPONSE = `{
  "id": "chatcmpl-8f3a1c92d7e4",
  "object": "chat.completion",
  "created": 1767225600,
  "model": "${MODEL}",
  "choices": [
    {
      "index": 0,
      "message": {"role": "assistant", "content": "Hello! How can I help you today?"},
      "finish_reason": "stop"
    }
  ],
  "usage": {"prompt_tokens": 9, "completion_tokens": 10, "total_tokens": 19}
}`;

const L = (at: number, text: string, tone?: Tone): Step => ({ kind: 'line', at, screen: 'term', text, tone });

export const DEMO_STEPS: Step[] = [
  { kind: 'type', at: 300, screen: 'term', tone: 'cmd', text: '$ amicu apply' },
  { kind: 'status', at: 1000, status: 'starting' },
  L(1000, 'Plan: 1 action(s).'),
  L(1100, ' '),
  L(1200, '  + deployment.chat', 'add'),
  L(1300, '      action:   create'),
  L(1400, '      hardware: gpu.l4'),
  // plan.rs also prints a `launch:` line for some actions; trimmed here.
  L(1500, '      target:   modal (region eu-west)'),
  L(1600, '      api_key:  ****** (secret "CHAT_API_KEY", resolved at apply)'),
  L(2000, 'Apply these changes? [y/N] y'),
  L(4200, ' '),
  L(4200, 'Applied 1 deployment(s):'),
  L(4400, `  deployment.chat -> ${URL}`, 'muted'),
  { kind: 'status', at: 4400, status: 'online' },
  L(4700, '      curl:', 'muted'),
  ...CURL_ARGS.map((c, i) => L(4800 + i * 120, '        ' + c, 'muted')),
  { kind: 'screen', at: 7000, screen: 'call' },
  { kind: 'type', at: 7300, screen: 'call', tone: 'cmd', msPerChar: 6, text: '$ ' + CURL_ARGS.join('\n') },
  { kind: 'line', at: 10600, screen: 'call', text: RESPONSE },
];

/** Time at which every step has finished. */
export function demoDuration(steps: Step[]): number {
  let end = 0;
  for (const s of steps) {
    let e = s.at;
    if (s.kind === 'type') e += s.text.length * (s.msPerChar ?? 35);
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
    let shown = '';
    if (s.kind === 'type') {
      shown = s.text.slice(0, Math.max(0, Math.floor((t - s.at) / (s.msPerChar ?? 35))));
    } else {
      shown = started ? s.text : '';
    }
    rows.push({
      screen: s.screen,
      tone: s.tone ?? 'plain',
      shown,
      rest: s.text.slice(shown.length),
    });
  }
  return { screen, status, rows, done: t >= demoDuration(steps) };
}

/** Plain-text transcript of both panes, for the screen-reader copy. */
export function demoTranscript(steps: Step[]): string {
  return steps
    .flatMap((s) => (s.kind === 'line' || s.kind === 'type' ? [s.text] : []))
    .join('\n');
}
