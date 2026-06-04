---
title: Linting & validation
description: The FH01–FH11 rules, lint --package / --shareable / --allow-fn, and CI output.
sidebar:
  order: 1
---

`octofhir-sof` has two offline checkers for ViewDefinitions:

- **`validate`** — structural spec invariants only, no FHIR package or database.
  Covers FH06–FH10.
- **`lint`** — schema-driven selector and generated-SQL checks against a FHIR
  package (`--package`), and/or the portable FHIRPath subset (`--shareable`).
  Covers FH01–FH05 (schema), FH06–FH10 (structure, also run by `validate`), and
  FH11 (shareable).

Both emit rustc-style diagnostics by default, or `--json` / `--sarif` (SARIF
2.1.0) for CI code scanning. Each `FH*` finding links to its
[rule reference](/sof/rules/) page.

## The rules

| Code | Severity | What it flags |
|------|----------|---------------|
| [FH01](/sof/rules/fh01/) | error   | Unknown FHIR element (with a did-you-mean suggestion) |
| [FH02](/sof/rules/fh02/) | warning | Choice element not narrowed with `ofType()` |
| [FH03](/sof/rules/fh03/) | warning | Complex element selected into a scalar column |
| [FH04](/sof/rules/fh04/) | warning | Array-valued element into a scalar column |
| [FH05](/sof/rules/fh05/) | warning | `Reference` into a scalar without `getReferenceKey()` |
| [FH06](/sof/rules/fh06/) | error   | Invalid SQL name (column / constant / view name) |
| [FH07](/sof/rules/fh07/) | error   | More than one of `forEach` / `forEachOrNull` / `repeat` |
| [FH08](/sof/rules/fh08/) | error   | Duplicate output column name |
| [FH09](/sof/rules/fh09/) | error   | `unionAll` branches with mismatched shape |
| [FH10](/sof/rules/fh10/) | error   | Missing `resource`, or a view that produces no columns |
| [FH11](/sof/rules/fh11/) | error/warn | FHIRPath outside the ShareableViewDefinition subset |

FH01–FH05 need a schema (`--package`); FH06–FH10 are structural; FH11 is opt-in
with `--shareable`.

## Schema-driven linting

```sh
octofhir-sof lint view.json --package hl7.fhir.r4.core
octofhir-sof lint view.json --package hl7.fhir.r5.core --version 5.0.0
```

`--package` enables the selector checks (FH01–FH05) by walking each FHIRPath
selector against the real shape of the resource. With `--version`, the package
is installed through the canonical manager if missing; without it, only an
already-present package is used (offline). See
[FHIR versions](/sof/guides/fhir-versions/).

## Shareable subset (`--shareable`)

The Shareable View Definition profile requires runners to implement only a
minimal FHIRPath subset, so a view that stays inside it runs unchanged on any
conformant engine. FH11 walks every FHIRPath expression in the view and flags
anything outside that subset:

```sh
octofhir-sof lint view.json --shareable
```

This check needs **no package** — it is purely a FHIRPath analysis. The required
subset allows the functions `where`, `exists`, `empty`, `extension`, `ofType`,
`first` (plus the SQL-on-FHIR `getResourceKey`/`getReferenceKey`), the boolean
operators `and`/`or`/`not`, math `+ - * /`, the comparisons `= != > <=`, the
indexer `[]`, and String / Integer / Decimal / Boolean literals. The
experimental `join`, `lowBoundary`, `highBoundary` warn; everything else is an
error.

Exempt an engine-registered custom FHIRPath function so the strict pass does not
hard-fail it (repeatable):

```sh
octofhir-sof lint view.json --shareable --allow-fn myCustomFn
```

You can combine `--package` and `--shareable` in one run.
