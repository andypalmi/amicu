// Pure, framework-free helpers for rendering the wizard's preset data. Kept free
// of React so the label maps stay in one place and can be unit-tested.

import type { components } from '@amicu/api-client';

export type PresetSummaryDto = components['schemas']['PresetSummaryDto'];
export type PresetDetailDto = components['schemas']['PresetDetailDto'];
export type PresetConstraintDto = components['schemas']['PresetConstraintDto'];
export type WizardAnswersDto = components['schemas']['WizardAnswersDto'];
export type EmitResponseDto = components['schemas']['EmitResponseDto'];
export type WorkloadEstimateDto = components['schemas']['WorkloadEstimateDto'];
export type EstimateRangeDto = components['schemas']['EstimateRangeDto'];
export type ModalityDto = components['schemas']['ModalityDto'];
export type TierDto = components['schemas']['TierDto'];
export type RuntimeFamilyDto = components['schemas']['RuntimeFamilyDto'];

/** Human label for a runtime family. */
function runtimeFamilyLabel(family: RuntimeFamilyDto): string {
  switch (family) {
    case 'llama_cpp':
      return 'llama.cpp';
    case 'vllm':
      return 'vLLM';
    default:
      return family;
  }
}

/**
 * A short, human-readable label for a preset's hard constraint. The constraint
 * is either one of a fixed set of string tags or a `{ runtime_family }` object.
 */
export function constraintLabel(constraint: PresetConstraintDto): string {
  if (typeof constraint === 'object') {
    return `Runtime: ${runtimeFamilyLabel(constraint.runtime_family)}`;
  }
  switch (constraint) {
    case 'eu_residency_only':
      return 'EU residency only';
    case 'quantization_required':
      return 'Quantization required';
    case 'local_only':
      return 'Local only';
    case 'external_providers_forbidden':
      return 'No external providers';
    default:
      return constraint;
  }
}

/** Human label for a modality tag. */
export function modalityLabel(modality: ModalityDto): string {
  switch (modality) {
    case 'text':
      return 'Text';
    case 'vision':
      return 'Vision';
    case 'audio':
      return 'Audio';
    case 'embedding':
      return 'Embedding';
    default:
      return modality;
  }
}

/** Human label for a product tier. */
export function tierLabel(tier: TierDto): string {
  switch (tier) {
    case 'community':
      return 'Community';
    case 'pro':
      return 'Pro';
    case 'enterprise':
      return 'Enterprise';
    case 'hosted_cloud':
      return 'Hosted Cloud';
    default:
      return tier;
  }
}

/** Whether a preset commits to EU residency (used to seed the residency answer). */
export function presetRequiresEuResidency(summary: PresetSummaryDto): boolean {
  return summary.constraints.some(
    (c) => typeof c === 'string' && c === 'eu_residency_only',
  );
}

/** Render a millisecond estimate range as e.g. `"120 - 340 ms"`. */
export function formatRangeMs(range: EstimateRangeDto): string {
  const low = Math.round(range.low);
  const high = Math.round(range.high);
  return low === high ? `${low} ms` : `${low} - ${high} ms`;
}
