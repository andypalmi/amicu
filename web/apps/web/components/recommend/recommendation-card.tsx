import { Badge, Card, CardContent, Separator } from '@amicu/ui';
import {
  confidenceMeta,
  bottleneckLabel,
  directionMeta,
  formatEvidence,
  type Recommendation,
} from '@/lib/recommend/format';

// The shared advisory recommendation/suggestion card. The recommend console
// (as a "recommendation") and the monitor dashboard (as a "suggestion") consume
// the same RecommendationDto and rendered a byte-identical card each - this is
// the single presentation both now share so they cannot drift. Presentation
// only: every badge, section, and conditional comes straight from the
// API-derived recommendation; nothing here authors or reorders advice.
export function RecommendationCard({ rec }: { rec: Recommendation }) {
  const confidence = confidenceMeta(rec.confidence);
  return (
    <Card className="text-sm">
      <CardContent className="space-y-3 p-4">
        <header className="flex flex-wrap items-center gap-2">
          <Badge variant="outline">{bottleneckLabel(rec.bottleneck)}</Badge>
          <Badge variant={confidence.tone}>{confidence.label}</Badge>
          {rec.advisory ? <Badge variant="secondary">Advisory</Badge> : null}
          <span className="ml-auto text-xs text-muted-foreground">
            Requires {rec.required_tier}
          </span>
        </header>

        {/* Rationale - authored by the API, shown verbatim. */}
        <p>{rec.summary}</p>

        {rec.evidence.length > 0 ? (
          <div className="space-y-1.5">
            <Separator />
            <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
              Evidence
            </h3>
            <ul className="space-y-1">
              {rec.evidence.map((ev, i) => (
                <li key={`${ev.metric}-${i}`} className="flex flex-wrap items-center gap-2">
                  <code className="rounded bg-muted px-1.5 py-0.5 font-mono text-[11px]">
                    {formatEvidence(ev)}
                  </code>
                  <span className="text-xs text-muted-foreground">
                    ({ev.comparison === 'above' ? 'above' : 'below'} threshold)
                  </span>
                </li>
              ))}
            </ul>
          </div>
        ) : null}

        {rec.changes.length > 0 ? (
          <div className="space-y-1.5">
            <Separator />
            <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
              Suggested changes
            </h3>
            <ul className="space-y-1">
              {rec.changes.map((change, i) => {
                const meta = directionMeta(change.direction);
                return (
                  <li key={`${change.action}-${i}`} className="flex items-start gap-2">
                    <span className="font-mono" aria-hidden>
                      {meta.sigil}
                    </span>
                    <span>
                      <span className="font-medium">{meta.label}</span>{' '}
                      <code className="font-mono text-xs">{change.action}</code> - {change.detail}
                    </span>
                  </li>
                );
              })}
            </ul>
          </div>
        ) : null}

        {rec.rejected.length > 0 ? (
          <div className="space-y-1.5">
            <Separator />
            <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
              Rejected alternatives
            </h3>
            <ul className="space-y-1">
              {rec.rejected.map((alt, i) => (
                <li key={`${alt.change}-${i}`} className="text-muted-foreground">
                  <span className="font-medium text-foreground">{alt.change}</span> - {alt.reason}
                </li>
              ))}
            </ul>
          </div>
        ) : null}

        {rec.assumptions.length > 0 ? (
          <div className="space-y-1.5">
            <Separator />
            <h3 className="text-xs font-medium uppercase tracking-wide text-muted-foreground">
              Assumptions
            </h3>
            <ul className="list-inside list-disc space-y-1 text-xs text-muted-foreground">
              {rec.assumptions.map((a, i) => (
                <li key={i}>{a.statement}</li>
              ))}
            </ul>
          </div>
        ) : null}
      </CardContent>
    </Card>
  );
}
