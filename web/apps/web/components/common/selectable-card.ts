// Shared "selected" treatment for the wizard's selectable card grids (the
// preset gallery and the guided wizard's candidate grid): a subtle border +
// ring highlight on the active card. Centralised so both grids stay in sync
// instead of each repeating the same class string.
export function selectableCardClassName(active: boolean): string {
  return active ? 'border-foreground/40 ring-1 ring-foreground/20' : '';
}
