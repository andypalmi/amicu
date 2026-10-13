import { ogImage, ogSize } from '@/lib/og-image';

export const alt = 'amicu: infrastructure as code for self-hosted LLMs';
export const size = ogSize;
export const contentType = 'image/png';

export default function Image() {
  return ogImage();
}
