---
title: Conformance
description: 144/144 on both execution paths against the official SQL on FHIR suite, plus every example of the 3.0 guide.
sidebar:
  order: 4
---

`octofhir-sof` targets SQL on FHIR 3.0 and runs the official
[FHIR/sql-on-fhir.js](https://github.com/FHIR/sql-on-fhir.js) content-test
suite (vendored under `crates/octofhir-sof/tests/spec/`). **Both** execution
paths — the database-free in-memory evaluator and the generated PostgreSQL —
pass the full suite:

```text
TOTAL  144/144
```

The DuckDB SQL path is additionally verified through the `duckdb` CLI.

The 3.0 guide's examples (`tests/fixtures/v3-examples`) are tests as well:
every SQLQuery and SQLView Library reads in both dialects, and every
ViewDefinition passes the 3.0 structural rules, compiles and lowers to SQL.

## Running the suite

```sh
just conformance         # in-memory (no database)
just conformance-pg      # against PostgreSQL (needs Docker)
just conformance-duckdb  # against DuckDB (needs the `duckdb` CLI)
just conformance-report  # emit a result file for the sql-on-fhir.org registry
```

Refresh the vendored tests with `just update-spec-tests`.

## The result file for the registry

`sql-on-fhir.org` aggregates a public registry of implementations; each entry
points at a `testResultsUrl` whose document is keyed by test-file name with a
`tests` array of `{ name, result: { passed, reason? } }`. `just
conformance-report` writes `conformance-results.json` in exactly that shape (set
a custom path with `out=results.json`).

For the full result-file format and how to submit the OctoFHIR SoF entry to the
implementation registry, see
[CONFORMANCE.md](https://github.com/octofhir/sof/blob/main/CONFORMANCE.md) in the
repository.
