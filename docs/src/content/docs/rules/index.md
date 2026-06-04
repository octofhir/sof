---
title: Lint rules
description: Every octofhir-sof FHIR lint rule, with semantics and examples.
---

`octofhir-sof` ships 11 FHIR-level lint rules (FH01–FH11). Each has a dedicated
page with what it flags, an example that triggers it, and how to fix or silence
it. They are reported by `octofhir-sof validate` (structural rules) and
`octofhir-sof lint` (schema-driven and shareable rules). Each finding links to
its rule page via its `help_url`.

The rules cite the
[SQL on FHIR v2 specification](https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/)
where relevant.

## Selector rules (need `lint --package`)

| Code | Severity | Description |
|------|----------|-------------|
| [FH01](/sof/rules/fh01/) | error   | Unknown FHIR element (with a did-you-mean suggestion) |
| [FH02](/sof/rules/fh02/) | warning | Choice element not narrowed with `ofType()` |
| [FH03](/sof/rules/fh03/) | warning | Complex element selected into a scalar column |
| [FH04](/sof/rules/fh04/) | warning | Array-valued element into a scalar column |
| [FH05](/sof/rules/fh05/) | warning | `Reference` into a scalar without `getReferenceKey()` |

## Structural rules (run by `validate` and `lint`)

| Code | Severity | Description |
|------|----------|-------------|
| [FH06](/sof/rules/fh06/) | error | Invalid SQL name (column / constant / view name) |
| [FH07](/sof/rules/fh07/) | error | More than one of `forEach` / `forEachOrNull` / `repeat` per select |
| [FH08](/sof/rules/fh08/) | error | Duplicate output column name |
| [FH09](/sof/rules/fh09/) | error | `unionAll` branches with mismatched shape |
| [FH10](/sof/rules/fh10/) | error | Missing `resource`, or a view that produces no columns |

## Portability rule (opt-in with `lint --shareable`)

| Code | Severity | Description |
|------|----------|-------------|
| [FH11](/sof/rules/fh11/) | error / warning | FHIRPath outside the ShareableViewDefinition required subset |

See the [linting guide](/sof/guides/linting/) for how to run them.
