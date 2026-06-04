---
title: Quickstart
description: Run a view over NDJSON, generate SQL, validate and lint.
---

After [installing](/sof/install/), you need a `ViewDefinition`. Save this as
`patient_view.json`:

```json
{
  "resourceType": "ViewDefinition",
  "name": "patient_names",
  "resource": "Patient",
  "select": [
    {
      "column": [
        { "name": "id", "path": "id" },
        { "name": "gender", "path": "gender" },
        { "name": "family", "path": "name.first().family" }
      ]
    }
  ]
}
```

## Run a view over FHIR data — no database

Point `run` at a ViewDefinition and some resources (`--input` takes an NDJSON
file, a Bundle, a JSON resource/array, a directory, or `-` for stdin):

```sh
octofhir-sof run patient_view.json --input patients.ndjson
```

```text
id,gender,family
pt-1,female,Chalmers
pt-2,male,Levin
```

Choose the output format with `--output csv|ndjson|json` (`parquet` with the
Parquet feature). Streaming NDJSON in and NDJSON out runs in bounded memory:

```sh
cat export/*.ndjson | octofhir-sof run patient_view.json --input - --output ndjson
```

Run every view in a directory in one pass, one output file per view:

```sh
octofhir-sof run views/ --input data/ --output csv --out out/
```

## Generate SQL

`generate` emits a PostgreSQL JSONB `SELECT` by default, or DuckDB JSON with
`--dialect duckdb`:

```sh
octofhir-sof generate patient_view.json                  # PostgreSQL
octofhir-sof generate patient_view.json --dialect duckdb # DuckDB
```

Or a `CREATE TABLE` for the view's columns (`--ddl`, dialect `ansi` | `postgres`
| `duckdb`):

```sh
octofhir-sof generate patient_view.json --ddl --dialect postgres
```

See [SQL dialects](/sof/guides/sql-dialects/) for the differences.

## Validate (offline, no package)

`validate` checks the ViewDefinition's structure against the spec — SQL-safe
names, one iteration construct per select, unique column names, consistent
`unionAll` branches:

```sh
octofhir-sof validate patient_view.json
octofhir-sof validate patient_view.json --sarif > results.sarif
```

## Lint

Schema-driven linting checks selectors against a FHIR package; `--shareable`
checks the portable FHIRPath subset (offline, no package needed):

```sh
octofhir-sof lint patient_view.json --package hl7.fhir.r4.core
octofhir-sof lint patient_view.json --shareable
```

Browse every rule in the [rule reference](/sof/rules/), or read the
[linting guide](/sof/guides/linting/).

## Exit codes

`0` on success · `1` on validation errors, lint errors, test failures, or I/O
errors.
