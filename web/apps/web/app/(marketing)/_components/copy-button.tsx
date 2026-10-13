'use client';

import { useEffect, useRef, useState } from 'react';
import { Check, Clipboard } from 'lucide-react';

/**
 * Copy button for a CodePanel: copies the text of the first <pre> inside the
 * enclosing <figure>. Same look as the Fumadocs code block copy button; hides
 * itself when the panel has no <pre> (for example the chat illustration).
 */
export function CopyButton() {
  const ref = useRef<HTMLButtonElement>(null);
  const [checked, setChecked] = useState(false);
  // Rendered hidden until mounted and a <pre> is detected, so panels without
  // code never flash a button.
  const [visible, setVisible] = useState(false);

  useEffect(() => {
    if (ref.current?.closest('figure')?.querySelector('pre')) setVisible(true);
  }, []);

  return (
    <button
      ref={ref}
      style={visible ? undefined : { display: 'none' }}
      type="button"
      data-checked={checked || undefined}
      aria-label={checked ? 'Copied text' : 'Copy text'}
      className="-me-2 inline-flex size-7 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-accent-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring data-[checked]:text-accent-foreground [&_svg]:size-3.5"
      onClick={() => {
        const pre = ref.current?.closest('figure')?.querySelector('pre');
        if (!pre) return;
        navigator.clipboard
          .writeText(pre.textContent ?? '')
          .then(() => {
            setChecked(true);
            window.setTimeout(() => setChecked(false), 1500);
          })
          .catch(() => {});
      }}
    >
      {checked ? <Check aria-hidden /> : <Clipboard aria-hidden />}
    </button>
  );
}
