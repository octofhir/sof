---
title: SQL dialects
description: PostgreSQL vs DuckDB SELECT generation and CREATE TABLE DDL dialects.
sidebar:
  order: 2
---

`octofhir-sof generate` lowers a `ViewDefinition` to SQL through a single
dialect-parametrized generator (`SqlGenerator::with_dialect`). Both the
PostgreSQL and DuckDB outputs pass the full content-test suite — DuckDB is
verified end-to-end through the `duckdb` CLI.

## SELECT generation

```sh
octofhir-sof generate view.json                  # PostgreSQL JSONB (default)
octofhir-sof generate view.json --dialect duckdb # DuckDB JSON
```

- **`postgres`** (the default; `ansi` is treated as postgres) — emits a query
  over `jsonb` columns using PostgreSQL's JSON operators and functions.
- **`duckdb`** — emits a query using DuckDB's JSON functions.

The two share the same FHIRPath lowering; only the JSON access syntax differs.
The in-memory engine (`run --input`, `execute`) needs no SQL at all.

## CREATE TABLE DDL

With `--ddl`, `generate` emits a `CREATE TABLE` for the view's output columns
instead of the `SELECT`. The DDL dialect selects the column types:

```sh
octofhir-sof generate view.json --ddl                    # ansi (spec types)
octofhir-sof generate view.json --ddl --dialect postgres # PostgreSQL types
octofhir-sof generate view.json --ddl --dialect duckdb   # DuckDB types
```

- **`ansi`** (default) — the SQL-on-FHIR spec's column types.
- **`postgres`** / **`duckdb`** — the corresponding native type for each column.

The table name defaults to the ViewDefinition's `name`, falling back to
`<resource>_view`; override it with `--table <name>`.
