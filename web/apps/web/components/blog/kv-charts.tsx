import { cn } from '@amicu/ui';

// Figures for the KV cache post. Values are bytes of context memory per token,
// from each model's config.json (see the post's method section); DeepSeek is at
// its native FP4 cache, the others at FP8. Fixed per-sequence state (sliding
// windows, linear-attention state) is added once per conversation.
const models = [
  { name: 'DeepSeek V4.1 Flash', perToken: 890, fixedGB: 0.0029, highlight: true },
  { name: 'Kimi K3.1 Pro', perToken: 18_912, fixedGB: 0.449 },
  { name: 'Qwen3.8 2.4T', perToken: 47_104, fixedGB: 0.587, note: '*' },
  { name: 'GLM-5.3', perToken: 53_940, fixedGB: 0 },
] as const;

const gb = (m: (typeof models)[number], tokens: number) => (m.perToken * tokens) / 1e9 + m.fixedGB;

const MONO = 'font-mono text-xs uppercase tracking-wider text-muted-foreground';

/** Horizontal bars: context memory for one conversation of `tokens` tokens. */
export function KvBars({ tokens = 1_000_000 }: { tokens?: number }) {
  const base = gb(models[0], tokens);
  const max = Math.max(...models.map((m) => gb(m, tokens)));
  return (
    <figure className="not-prose my-8">
      <figcaption className={MONO}>Context memory for one 1M-token conversation, GB</figcaption>
      <ul className="mt-4 flex flex-col gap-4">
        {models.map((m) => {
          const value = gb(m, tokens);
          const times = Math.round(value / base);
          return (
            <li key={m.name}>
              <div className="flex items-baseline justify-between gap-4 text-sm">
                <span className={'highlight' in m ? 'text-foreground' : 'text-muted-foreground'}>
                  {m.name}
                  {'note' in m ? m.note : null}
                  {'highlight' in m ? null : (
                    <span className="ml-3 font-mono text-xs text-accent-text">{times}× more</span>
                  )}
                </span>
                <span className="font-semibold tabular-nums text-foreground">{value.toFixed(1)}</span>
              </div>
              <div className="mt-1.5 h-3">
                <div
                  className={cn(
                    'h-3 min-w-1 rounded-r-sm',
                    'highlight' in m ? 'bg-primary' : 'bg-muted-foreground/40',
                  )}
                  style={{ width: `${(value / max) * 100}%` }}
                />
              </div>
            </li>
          );
        })}
      </ul>
    </figure>
  );
}

/** How many conversations of `tokens` tokens fit in `budgetGB` of free GPU memory. */
export function SessionTiles({ tokens = 262_144, budgetGB = 80 }: { tokens?: number; budgetGB?: number }) {
  return (
    <figure className="not-prose my-8">
      <figcaption className={MONO}>
        {Math.round(tokens / 1000)}k-token sessions in {budgetGB} GB of free GPU memory
      </figcaption>
      <ul className="mt-4 grid grid-cols-2 gap-3 sm:grid-cols-4">
        {models.map((m) => (
          <li
            key={m.name}
            className={cn(
              'rounded-md border p-4',
              'highlight' in m ? 'border-primary' : 'border-border',
            )}
          >
            <div
              className={cn(
                'text-3xl font-semibold tabular-nums tracking-tight',
                'highlight' in m ? 'text-accent-text' : 'text-foreground',
              )}
            >
              {Math.floor(budgetGB / gb(m, tokens))}
            </div>
            <div className="mt-1 text-sm text-muted-foreground">{m.name}</div>
          </li>
        ))}
      </ul>
    </figure>
  );
}

// DeepSeek V4.1 Flash's 40 layers (config: kv_source_layers 2, 8, 14, 20).
const SOURCE = new Set([2, 8, 14, 20]);
const SWA_ONLY = new Set([0, 1]);

/** The 40 layers, marking the four that store context and the ones that reuse it. */
export function LayerMap() {
  const cell = (i: number) => (
    <div
      key={i}
      title={`Layer ${i}`}
      className={cn(
        'flex aspect-square items-center justify-center rounded-sm font-mono text-[10px] tabular-nums',
        SOURCE.has(i)
          ? 'bg-primary text-primary-foreground'
          : SWA_ONLY.has(i)
            ? 'border border-dashed border-border text-muted-foreground'
            : 'bg-muted-foreground/15 text-muted-foreground',
      )}
    >
      {i}
    </div>
  );
  const half = (label: string, from: number) => (
    <div>
      <div className={MONO}>{label}</div>
      <div className="mt-2 grid grid-cols-10 gap-1">
        {Array.from({ length: 20 }, (_, k) => cell(from + k))}
      </div>
    </div>
  );
  return (
    <figure className="not-prose my-8">
      <div className="flex flex-col gap-4">
        {half('Encoder · layers 0–19 · 2 tokens per stored entry', 0)}
        {half('Decoder · layers 20–39 · 1 token per stored entry', 20)}
      </div>
      <figcaption className="mt-4 flex flex-wrap gap-x-5 gap-y-2 text-sm text-muted-foreground">
        <span className="flex items-center gap-2">
          <span className="size-3 rounded-sm bg-primary" /> stores the context
        </span>
        <span className="flex items-center gap-2">
          <span className="size-3 rounded-sm bg-muted-foreground/15" /> reuses a stored layer
        </span>
        <span className="flex items-center gap-2">
          <span className="size-3 rounded-sm border border-dashed border-border" /> recent tokens only
        </span>
      </figcaption>
    </figure>
  );
}
