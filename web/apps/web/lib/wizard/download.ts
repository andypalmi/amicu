// Trigger a client-side file download from in-memory text, with no network hop -
// the strict CSP forbids external hosts, but a same-origin Blob URL is fine. Used
// by the wizard result panel to save the emitted HCL as `stack.amicu.hcl`.
export function downloadText(filename: string, text: string): void {
  const blob = new Blob([text], { type: 'text/plain;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = filename;
  document.body.appendChild(anchor);
  anchor.click();
  anchor.remove();
  // Release the object URL on the next tick so the click has already started.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}
