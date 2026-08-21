import { Alert, AlertDescription, Badge, Card, CardContent } from '@amicu/ui';
import { actionKindMeta, summarizePlan, type Plan } from '@/lib/engine/format';

// Renders a Plan as a legible diff: each action shows its kind (a distinctly
// styled badge + sigil), address, target placement, and any required manual
// action / secret names. A policy outcome, if present, is surfaced up top.
export function PlanPanel({ plan }: { plan: Plan }) {
  const summary = summarizePlan(plan.actions);

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        {plan.actions.length === 0 ? (
          <Badge variant="secondary">No changes</Badge>
        ) : (
          summary.map(({ kind, count }) => {
            const meta = actionKindMeta(kind);
            return (
              <Badge key={kind} variant={meta.tone}>
                {meta.sigil} {count} {meta.label.toLowerCase()}
              </Badge>
            );
          })
        )}
      </div>

      {plan.policy ? (
        <Alert
          variant={plan.policy.allowed ? 'success' : 'destructive'}
          className={plan.policy.allowed ? 'bg-success/10' : 'bg-destructive/10'}
        >
          <AlertDescription
            className={plan.policy.allowed ? 'text-success' : 'text-destructive'}
          >
            <span className="font-medium">
              Policy: {plan.policy.allowed ? 'allowed' : 'denied'}
            </span>
            {plan.policy.denials && plan.policy.denials.length > 0 ? (
              <ul className="mt-1 list-inside list-disc text-xs">
                {plan.policy.denials.map((reason, i) => (
                  <li key={i}>{reason}</li>
                ))}
              </ul>
            ) : null}
            {plan.policy.risks && plan.policy.risks.length > 0 ? (
              <ul className="mt-1 list-inside list-disc text-xs opacity-80">
                {plan.policy.risks.map((risk, i) => (
                  <li key={i}>{risk}</li>
                ))}
              </ul>
            ) : null}
          </AlertDescription>
        </Alert>
      ) : null}

      {plan.actions.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          The current configuration matches the prior state. Nothing to apply.
        </p>
      ) : (
        <div className="space-y-2">
          {plan.actions.map((action, i) => {
            const meta = actionKindMeta(action.kind);
            const placement = [action.provider, action.region, action.hardware]
              .filter(Boolean)
              .join(' · ');
            return (
              <Card key={`${action.address}-${i}`} className="text-sm">
                <CardContent className="p-3">
                  <div className="flex flex-wrap items-center gap-2">
                    <Badge variant={meta.tone}>
                      <span className="font-mono">{meta.sigil}</span>
                      <span className="ml-1">{meta.label}</span>
                    </Badge>
                    <code className="font-mono text-xs">{action.address}</code>
                  </div>
                  {placement ? (
                    <p className="mt-1.5 text-xs text-muted-foreground">{placement}</p>
                  ) : null}
                  {action.required_action ? (
                    <p className="mt-1.5 text-xs">
                      <span className="font-medium">Manual step:</span> {action.required_action}
                    </p>
                  ) : null}
                  {action.required_secrets && action.required_secrets.length > 0 ? (
                    <p className="mt-1 flex flex-wrap items-center gap-1.5 text-xs">
                      <span className="text-muted-foreground">Secrets:</span>
                      {action.required_secrets.map((name) => (
                        <code
                          key={name}
                          className="rounded bg-muted px-1 py-0.5 font-mono text-[11px]"
                        >
                          {name}
                        </code>
                      ))}
                    </p>
                  ) : null}
                </CardContent>
              </Card>
            );
          })}
        </div>
      )}
    </div>
  );
}
