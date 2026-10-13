// Declarative timeline + pure reducer for the home apply demo.
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

interface Base {
  at: number;
  screen: Screen;
  tone?: Tone;
}
export type Step =
  | (Base & { kind: 'type'; text: string; msPerChar?: number })
  | (Base & { kind: 'line'; text: string })
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
  rows: Row[];
  done: boolean;
}

const URL = 'https://<workspace>--amicu-chat-server.<region>.modal.direct';
const MODEL = 'RedHatAI/Qwen3.5-9B-FP8-dynamic';
const BODY = `{"model": "${MODEL}", "messages": [{"role": "user", "content": "Hello"}]}`;
const CURL_ARGS = [
  `curl '${URL}/v1/chat/completions' \\`,
  '  -H "Authorization: Bearer $CHAT_API_KEY" -H "Modal-Key: $MODAL_PROXY_TOKEN_ID" \\',
  '  -H "Modal-Secret: $MODAL_PROXY_TOKEN_SECRET" -H "Content-Type: application/json" \\',
  `  -d '${BODY}'`,
];

// Non-streaming body, as vLLM returns it: the curl above sends no `stream`.
// Whitespace is compacted (one line per top-level key) to stay short on phones.
const RESPONSE = `{
"id": "chatcmpl-8f3a1c92d7e4",
"object": "chat.completion",
"created": 1767225600,
"model": "${MODEL}",
"choices": [{"index": 0, "message": {"role": "assistant", "content": "Hello! How can I help you today?"}, "finish_reason": "stop"}],
"usage": {"prompt_tokens": 9, "completion_tokens": 10, "total_tokens": 19}
}`;

const L = (at: number, text: string, tone?: Tone): Step => ({ kind: 'line', at, screen: 'term', text, tone });

export const DEMO_STEPS: Step[] = [
  { kind: 'type', at: 100, screen: 'term', tone: 'cmd', msPerChar: 30, text: '$ amicu apply' },
  L(650, 'Plan: 1 action(s).'),
  L(730, ' '),
  L(810, '  + deployment.chat', 'add'),
  L(890, '      action:   create'),
  L(970, '      hardware: gpu.l4'),
  // plan.rs also prints a `launch:` line for some actions; trimmed here.
  L(1050, '      target:   modal (region eu-west)'),
  L(1130, '      api_key:  ****** (secret "CHAT_API_KEY", resolved at apply)'),
  L(1500, 'Apply these changes? [y/N] y'),
  // Output lands ~450ms after the `y` (the owner's budget is 500ms).
  L(1950, ' '),
  L(1950, 'Applied 1 deployment(s):'),
  L(2050, `  deployment.chat -> ${URL}`, 'muted'),
  // apply.rs then prints a `curl:` / `python (openai):` usage snippet; trimmed here,
  // the request is shown once, in the next pane.
  { kind: 'screen', at: 2800, screen: 'call' },
  // The 4-line command types in well under a second (a few chars per frame).
  { kind: 'type', at: 3000, screen: 'call', tone: 'cmd', msPerChar: 2.5, text: '$ ' + CURL_ARGS.join('\n') },
  { kind: 'line', at: 4200, screen: 'call', text: RESPONSE },
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
  const rows: Row[] = [];
  for (const s of steps) {
    const started = t >= s.at;
    if (s.kind === 'screen') {
      if (started) screen = s.screen;
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
  return { screen, rows, done: t >= demoDuration(steps) };
}

/** Plain-text transcript of both panes, for the screen-reader copy. */
export function demoTranscript(steps: Step[]): string {
  return steps
    .flatMap((s) => (s.kind === 'line' || s.kind === 'type' ? [s.text] : []))
    .join('\n');
}
