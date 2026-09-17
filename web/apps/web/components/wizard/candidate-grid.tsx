'use client';

import { Badge, Button, Card, CardContent } from '@amicu/ui';
import { EmptyState } from '@/components/common/empty-state';
import { selectableCardClassName } from '@/components/common/selectable-card';
import { archetypeLabel, type CandidateDto } from '@/lib/recommend/candidates';

// The guided wizard's candidates step (andypalmi/amicu-planning#85): renders
// the ranked `CandidateDto[]` from `POST /recommend/candidates` as a card
// grid. Selection is tracked by the caller as `selectedIndex: number | null`
// (fixing a review nit in the #31 exercise route's candidate-picker.tsx, which
// compared `selected === candidate` by reference identity - fragile once a
// candidate object could be replaced by an equal-looking one from a re-fetch).
export function CandidateGrid({
  candidates,
  selectedIndex,
  onSelect,
}: {
  candidates: CandidateDto[];
  selectedIndex: number | null;
  onSelect: (index: number) => void;
}) {
  if (candidates.length === 0) {
    return (
      <EmptyState
        title="No candidates"
        description="No candidate satisfied every hard constraint for these answers. Go back and relax one, or try a different preset."
      />
    );
  }

  return (
    <div className="grid gap-3 sm:grid-cols-2">
      {candidates.map((c, i) => {
        const active = selectedIndex === i;
        return (
          <Card key={i} className={selectableCardClassName(active)}>
            <CardContent className="space-y-2 p-4">
              <div className="flex items-center justify-between gap-2">
                <span className="text-sm font-medium">{archetypeLabel(c.archetype)}</span>
                <Badge variant="outline">score {c.score.total.toFixed(2)}</Badge>
              </div>
              {c.rationale[0] ? (
                <p className="text-xs text-muted-foreground">{c.rationale[0]}</p>
              ) : null}
              <Button
                type="button"
                size="sm"
                variant={active ? 'default' : 'outline'}
                className="w-full"
                onClick={() => onSelect(i)}
              >
                {active ? 'Selected' : 'Review this candidate'}
              </Button>
            </CardContent>
          </Card>
        );
      })}
    </div>
  );
}
