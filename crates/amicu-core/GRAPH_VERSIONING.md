# Canonical Graph Versioning & Migration Policy

The canonical resource graph is versioned with `schema_version`, a
[semantic version](https://semver.org/) string. The current value is the
`GRAPH_SCHEMA_VERSION` constant in `amicu-core` (`crate::graph`). Rust types
are the source of truth; canonical JSON is their serialization, and every
serialized graph carries the `schema_version` it was written against.

## Version scheme

`MAJOR.MINOR.PATCH` applied to the JSON shape of the graph and its resources:

- **MAJOR** — a breaking change (see below). A reader for version `N` is not
  expected to parse a graph written for version `N+1`.
- **MINOR** — a backward-compatible, additive change. Older readers ignore
  what they do not recognize; newer readers accept older graphs unchanged.
- **PATCH** — clarifications and non-normative fixes with no wire effect
  (documentation, comments, tightened validation that accepts the same
  inputs).

## Breaking vs. additive

**Additive (MINOR)** — no migration required:

- Adding a new resource kind.
- Adding an **optional** field (`Option<_>`, or one with `#[serde(default)]`).
- Adding a new variant to an open-ended enum that consumers treat as
  non-exhaustive.
- Relaxing a constraint so previously-invalid inputs become valid.

**Breaking (MAJOR)** — requires a migration:

- Removing or renaming a field, resource kind, or enum variant.
- Making an optional field required, or changing a field's type or units.
- Changing the meaning of an existing field or the default of a defaulted one.
- Restructuring references between resources (e.g. how a `deployment` binds a
  `model`).

## Migrations

Migrations are stepwise functions between adjacent MAJOR versions
(`v1 -> v2 -> v3`), applied in sequence to upgrade an older graph. Each
migration:

1. Reads the graph together with its declared `schema_version`.
2. Transforms the data to the next version's shape.
3. Stamps the new `schema_version`.

Migrations are additive and never edited once released, so any historical
graph can be upgraded deterministically to the current version. A graph whose
`schema_version` is newer than the reader is rejected rather than guessed at.
(The migration runner itself lands in a later task; this document fixes the
policy those migrations must follow.)

## Compatibility guarantee

Within a single MAJOR version, any graph that a given release can write, every
later release of that MAJOR can read. Across a MAJOR bump, a released migration
path always exists from the previous MAJOR to the new one.
