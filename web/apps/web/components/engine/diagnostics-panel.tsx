import { Badge } from '@amicu/ui';
import {
  groupBySeverity,
  severityCounts,
  severityMeta,
  SEVERITY_ORDER,
  type DiagnosticReport,
} from '@/lib/engine/format';

// Renders a DiagnosticReport: a valid config with no diagnostics shows a green
// "valid" state; otherwise diagnostics are grouped error → warning → info.
export function DiagnosticsPanel({ report }: { report: DiagnosticReport }) {
  const counts = severityCounts(report.diagnostics);
  const groups = groupBySeverity(report.diagnostics);

  if (report.valid && report.diagnostics.length === 0) {
    return (
      <div className="rounded-lg border border-emerald-600/30 bg-emerald-500/10 p-4 text-sm text-emerald-700 dark:text-emerald-300">
        <span className="font-medium">Configuration is valid.</span> No diagnostics.
      </div>
    );
  }

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2 text-xs">
        <Badge variant={report.valid ? 'secondary' : 'destructive'}>
          {report.valid ? 'Valid' : 'Invalid'}
        </Badge>
        <span className="text-muted-foreground">
          {counts.error} error{counts.error === 1 ? '' : 's'}, {counts.warning} warning
          {counts.warning === 1 ? '' : 's'}, {counts.info} info
        </span>
      </div>

      <ul className="space-y-2">
        {SEVERITY_ORDER.flatMap((severity) =>
          groups[severity].map((d, i) => {
            const meta = severityMeta(severity);
            return (
              <li
                key={`${severity}-${i}-${d.code}`}
                className="rounded-lg border border-border bg-card p-3 text-sm"
              >
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant={meta.tone}>{meta.label}</Badge>
                  <code className="font-mono text-xs text-muted-foreground">{d.code}</code>
                  {d.resource ? (
                    <code className="font-mono text-xs text-muted-foreground">
                      {d.resource}
                      {d.path ? `.${d.path}` : ''}
                    </code>
                  ) : null}
                </div>
                <p className="mt-1.5">{d.message}</p>
                {d.suggested_fixes && d.suggested_fixes.length > 0 ? (
                  <p className="mt-1 text-xs text-muted-foreground">
                    {d.suggested_fixes.length} suggested fix
                    {d.suggested_fixes.length === 1 ? '' : 'es'}
                    {d.suggested_fixes[0]?.description
                      ? `: ${d.suggested_fixes[0].description}`
                      : ''}
                  </p>
                ) : null}
              </li>
            );
          }),
        )}
      </ul>
    </div>
  );
}
