import type { ChatSession } from '../_lib/landing';

/**
 * Static mock of a terminal chat client. Unbranded and illustrative: a framed
 * header, one user turn, one reply, an input box and a key hint. Plain CSS
 * borders keep it crisp and wrapping at any width.
 */
export function TerminalChat({ session }: { session: ChatSession }) {
  return (
    <div
      role="img"
      aria-label={`Example chat with ${session.model}. Prompt: ${session.user} Reply: ${session.reply.join(' ')}`}
      className="p-3 font-mono text-[0.8125rem] leading-6 sm:p-4"
    >
      <div aria-hidden className="rounded-[3px] border border-border">
        <div className="flex flex-col gap-x-3 border-b border-border px-3 py-2 sm:flex-row sm:flex-wrap sm:items-baseline">
          <span className="font-semibold text-foreground">{session.client}</span>
          <span className="break-all text-foreground">{session.model}</span>
          <span className="break-all text-muted-foreground">{session.host}</span>
        </div>

        <div className="flex flex-col gap-4 px-3 py-4">
          <p className="flex gap-2">
            <span className="text-accent-text">&gt;</span>
            <span className="min-w-0 break-words text-foreground">{session.user}</span>
          </p>
          <div className="flex flex-col gap-2 pl-4 text-foreground">
            {session.reply.map((line) => (
              <p key={line} className="break-words">
                {line}
              </p>
            ))}
            <p className="text-muted-foreground">{session.status}</p>
          </div>
        </div>

        <div className="border-t border-border p-3">
          <div className="flex gap-2 rounded-[3px] border border-border px-3 py-1.5">
            <span className="text-accent-text">&gt;</span>
            <span className="inline-block h-6 w-[0.6em] bg-foreground" />
          </div>
          <p className="mt-2 text-xs text-muted-foreground">{session.hint}</p>
        </div>
      </div>
    </div>
  );
}
