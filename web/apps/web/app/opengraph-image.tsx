import { ogImage, ogSize } from '@/lib/og-image';

export const alt = 'amicu: infrastructure as code for open-weight LLMs';
export const size = ogSize;
export const contentType = 'image/png';

export default function Image() {
  return ogImage();
}
