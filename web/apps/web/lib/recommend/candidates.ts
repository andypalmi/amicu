// Pure, framework-free helpers for the guided-wizard candidate flow
// (andypalmi/amicu-planning#31 questions/candidates, #35 customize). Kept free
// of React so the type re-exports and label/guard helpers can be unit-tested
// and reused by both the candidate picker and <CandidateReview>.

import type { components } from '@amicu/api-client';

export type QuestionDto = components['schemas']['QuestionDto'];
export type QuestionKindDto = components['schemas']['QuestionKindDto'];
export type CandidateArchetypeDto = components['schemas']['CandidateArchetypeDto'];
export type CandidateScoreDto = components['schemas']['CandidateScoreDto'];
export type CandidateDto = components['schemas']['CandidateDto'];
export type CandidatesResponseDto = components['schemas']['CandidatesResponseDto'];
export type CustomizeRequestDto = components['schemas']['CustomizeRequestDto'];
export type CustomizeResponseDto = components['schemas']['CustomizeResponseDto'];

/** Human label for a candidate archetype. */
export function archetypeLabel(archetype: CandidateArchetypeDto): string {
  switch (archetype) {
    case 'simplest':
      return 'Simplest';
    case 'balanced':
      return 'Balanced';
    case 'cost_optimized':
      return 'Cost-optimized';
    case 'performance_optimized':
      return 'Performance-optimized';
    case 'local_development':
      return 'Local development';
    default:
      return archetype;
  }
}

/** Narrow an unknown JSON body to a `QuestionDto[]` (GET /recommend/questions). */
export function looksLikeQuestions(body: unknown): body is QuestionDto[] {
  return (
    Array.isArray(body) &&
    body.every(
      (q) =>
        !!q &&
        typeof q === 'object' &&
        typeof (q as { id?: unknown }).id === 'string' &&
        typeof (q as { kind?: unknown }).kind === 'object',
    )
  );
}

/** Narrow an unknown JSON body to a `CandidatesResponseDto` (POST /recommend/candidates). */
export function looksLikeCandidatesResponse(body: unknown): body is CandidatesResponseDto {
  return (
    !!body &&
    typeof body === 'object' &&
    Array.isArray((body as { candidates?: unknown }).candidates)
  );
}

/** Narrow an unknown JSON body to a `CustomizeResponseDto` (POST /recommend/customize). */
export function looksLikeCustomizeResponse(body: unknown): body is CustomizeResponseDto {
  return (
    !!body &&
    typeof body === 'object' &&
    typeof (body as { hcl?: unknown }).hcl === 'string' &&
    typeof (body as { report?: unknown }).report === 'object'
  );
}
