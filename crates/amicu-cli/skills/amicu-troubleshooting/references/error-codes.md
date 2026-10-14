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
- `AMICU-SCHEMA-002` - the same resource id is declared in more than one config
  file.
- `AMICU-SCHEMA-003` - unknown attribute or block (usually a typo such as
  `contex_length`); the message names the block, the offending key, and a
  `did you mean` suggestion. Unknown keys are a hard error, never ignored.
  Free-form maps (`env {}`, `labels {}`) accept any key.
- `AMICU-REF-001` - a referenced resource id was not found.
- `AMICU-CAP-001` - capability mismatch (e.g. runtime cannot serve the artifact).
- `AMICU-CAP-002` - an `extra_args` flag duplicates a typed field's flag.
- `AMICU-CAP-003` - an `extra_args` flag or `env` key is denylisted.
- `AMICU-CAP-004` - info: `extra_args` is non-empty, so the memory/fit verdict
  is unverified.
- `AMICU-CAP-005` - a `llama_cpp` block sets both `chat_template` and
  `chat_template_file`.
- `AMICU-CAP-006` - warning: the artifact is GGUF but the runtime is vLLM
  (experimental there).
- `AMICU-CAP-007` - a `speculative` draft model is the deployment's own
  primary model.
- `AMICU-CAP-008` - invalid `scaling {}` values.
- `AMICU-CAP-009` - `scaling {}` on a provider with no scaling mapping.
- `AMICU-CAP-010` - warning: vLLM KV cache not estimated because the model
  lacks `n_layers`/`n_kv_heads`/`head_dim`; the fit check covers weights only.
  Declare the shape on the model.
- `AMICU-CAP-011` - a typed llama.cpp field is out of range or inconsistent
  (`top_p` outside `[0, 1]`, `ubatch_size` above `batch_size`,
  `parallel_slots = 0`, ...). The message names the field.
- `AMICU-MEM-001` / `AMICU-MEM-002` - capacity/memory fit failures.
- `AMICU-POLICY-001` - a governance/residency rule was violated.
- `AMICU-POLICY-004` - a secret-designated field (`api_key`, `token`, ...) was
  given as a literal. Use `secret("NAME")`.
- `AMICU-POLICY-005` - warning: free-text field or comment looks like a secret
  (key, JWT, PEM). Move it out; `# amicu:allow` on the line suppresses it.
- `AMICU-POLICY-006` - warning: a runtime `env` value looks like a secret. Move
  it to a `secret(...)` reference.
- `AMICU-POLICY-007` - a deployment reachable off-machine (e.g. `modal`) has no
  `api_key`. Add `api_key = secret("NAME")` and `export NAME=$(openssl rand -hex 32)`.
- `AMICU-POLICY-008` - the resolved endpoint API key is unusable (empty, or
  contains a comma, whitespace or non-ASCII). Regenerate it.
- `AMICU-STATE-001..004` - state lock/drift/read/write problems.
- `AMICU-PROVIDER-001` / `AMICU-PROVIDER-002` - provider provisioning/availability.

Severity, not just presence, decides pass/fail: only `error`-severity
diagnostics fail a command. Warnings are advisory but worth resolving.
