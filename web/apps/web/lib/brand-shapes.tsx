// Raw geometry of the Amicu monogram, shared by the inline React mark
// (components/common/brand.tsx) and the generated icon / Open Graph images
// (which render outside the app's component tree). Strokes inherit from the
// parent <svg>, so callers set stroke colour and width. Returned as a plain
// keyed array of intrinsic elements (not a component) because next/og's
// renderer only accepts intrinsic elements inside <svg>.
export function brandMarkShapes() {
  return [
    <rect key="r" x={2.5} y={2.5} width={19} height={19} rx={5.5} />,
    <path key="a" d="M7.5 16.5 12 7l4.5 9.5" />,
    <path key="b" d="M9.25 13h5.5" />,
  ];
}
