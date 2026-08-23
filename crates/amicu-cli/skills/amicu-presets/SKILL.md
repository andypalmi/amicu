---
name: amicu-presets
description: Use at the start of a guided Amicu stack design - gathering requirements (use case, sensitivity, geography, concurrency, latency, budget, existing hardware), then proposing candidate stacks with cost/performance estimates and confidence before writing HCL. Triggers on "help me set up", "recommend a stack", "which preset", or open-ended "I want to serve X".
---

# Amicu Presets & Guided Design

A preset is a versioned requirements template, not a copied snippet: it asks the
right questions, applies constraints, ranks candidates, and produces visible,
editable HCL with explained trade-offs. Use this skill to drive the guided flow.

## Gather requirements first

Ask only for what is missing:

- Use case / task and quality bar
- Data sensitivity and geography (residency, e.g. EU-only)
- Expected concurrency and latency preference
- Budget
- Existing hardware / provider, if any

## Guided flow

1. Read existing config and load the relevant narrow skills.
2. Fill the requirement gaps above.
3. Query presets/recommendations before inventing a stack (see command note).
4. Query model/hardware capability before choosing runtime/artifact
   (`amicu-model-selection`).
5. Generate or minimally update HCL (`amicu-dsl`).
6. `amicu fmt` + `amicu validate`; use only safe deterministic repairs.
7. `amicu plan`.
8. Present the plan with alternatives, assumptions, cost/performance ranges,
   confidence, and risk.
9. Apply only after explicit approval.

## Candidates and estimates

Offer 2-3 candidate stacks (e.g. local Apple Silicon vs cloud GPU), each with a
cost range, latency/throughput expectation, a confidence level, and its main
risk. Never present a single point estimate as certain. Respect residency:
never propose a candidate that violates declared constraints.

## Command note

`amicu presets list/show` and `amicu recommend` are the intended commands but
are NOT implemented yet (future tasks). Until they ship, drive the flow manually
using the reference catalog and the model-selection skill; do not fabricate
their JSON output.

## Reference

- `references/preset-catalog.md` - the initial preset shapes and when each fits.
