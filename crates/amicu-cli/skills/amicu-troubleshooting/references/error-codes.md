# Error-code catalog

Codes follow `AMICU-<CATEGORY>-<NNN>`. The category is the fastest triage
signal; the number identifies the specific check.

| Category | Meaning | Typical fix |
|---|---|---|
| `PARSE` | HCL syntax error | Fix syntax; `amicu fmt`; re-validate. |
| `SCHEMA` | Invalid/missing field or wrong type | Correct the field against resource-types. |
| `REF` | Reference to a non-existent id | Fix the `<kind>.<name>.id`. |
| `CAP` | Runtime/artifact/hardware capability mismatch | Change runtime, artifact, or hardware. |
| `MEM` | Model does not fit accelerator/host memory | Smaller quant, less context, or bigger hardware. |
| `POLICY` | Governance/residency/budget violation | Change provider/region/stack, not the rule. |
| `STATE` | State lock, drift, or corruption | Re-plan; recover only with explicit approval. |
| `PROVIDER` | Provisioning / availability failure | Check region capacity and credentials. |

## Representative codes

- `AMICU-PARSE-001` - HCL failed to parse.
- `AMICU-SCHEMA-001` - structural/schema validation failed.
- `AMICU-SCHEMA-002` - unknown attribute or block (usually a typo such as
  `contex_length`); the message names the block, the offending key, and a
  `did you mean` suggestion. Unknown keys are a hard error, never ignored.
  Free-form maps (`env {}`, `labels {}`) accept any key.
- `AMICU-REF-001` - a referenced resource id was not found.
- `AMICU-CAP-001` - capability mismatch (e.g. runtime cannot serve the artifact).
- `AMICU-MEM-001` / `AMICU-MEM-002` - capacity/memory fit failures.
- `AMICU-POLICY-001` - a governance/residency rule was violated.
- `AMICU-STATE-001..004` - state lock/drift/read/write problems.
- `AMICU-PROVIDER-001` / `AMICU-PROVIDER-002` - provider provisioning/availability.

Severity, not just presence, decides pass/fail: only `error`-severity
diagnostics fail a command. Warnings are advisory but worth resolving.
