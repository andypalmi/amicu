import { ogImage, ogSize } from '@/lib/og-image';

export const alt = 'Amicu: design, deploy, and operate LLM serving stacks';
export const size = ogSize;
export const contentType = 'image/png';

export default function Image() {
  return ogImage();
}
