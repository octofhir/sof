---
title: CLI reference
description: Every octofhir-sof subcommand and flag.
---

```text
octofhir-sof generate <view> [--ddl] [--dialect ansi|postgres|duckdb] [--table <name>]
octofhir-sof run      <view> [--input <file|dir|->] [--db <url>] [--output csv|ndjson|json|parquet] [--out <path>] [--parquet-temporal text|native] [--threads <n>]
octofhir-sof validate <view> [--json | --sarif]
octofhir-sof lint     <view> [--package <name>] [--version <v>] [--shareable] [--allow-fn <name>]… [--json | --sarif]
octofhir-sof test     <manifest>
```

`<view>` is a `ViewDefinition` JSON file (some commands also accept a directory
of them). Exit codes: `0` success · `1` validation errors, lint errors, test
failures, or I/O errors.

## Global flags

- `--no-color` — disable coloured output (also honoured via `NO_COLOR`).

## Commands

### `generate`

Generate SQL from a ViewDefinition, offline. Emits a PostgreSQL `SELECT` by
default; `--dialect duckdb` emits DuckDB SQL. With `--ddl`, emit a `CREATE TABLE`
for the output columns instead.

- `--ddl` — emit `CREATE TABLE` instead of the `SELECT`.
- `--dialect <d>` — for the SELECT: `postgres` (default) or `duckdb` (`ansi` is
  treated as postgres). For `--ddl`: `ansi` (default, spec types), `postgres` or
  `duckdb`. (The flag default is `ansi`.)
- `--table <name>` — table name for `--ddl` (defaults to the view's `name`, else
  `<resource>_view`).

### `run`

Execute a ViewDefinition and write the rows. Runs against FHIR files with
`--input` (no database) or against PostgreSQL with `--db`. A **directory** of
ViewDefinitions runs every view in one pass, one output file per view into
`--out`.

- `--input <file|dir|->` — FHIR resources with no database: an NDJSON file, a
  Bundle, a JSON resource/array, a directory of such files, or `-` for stdin.
  Conflicts with `--db`.
- `--db <url>` — PostgreSQL connection URL (or set `DATABASE_URL`).
- `--output <fmt>` — `csv` (default), `ndjson`, `json` (`parquet` with the
  Parquet feature).
- `--parquet-temporal <text|native>` — Parquet encoding; defaults to `text`
  (original FHIR strings). `native` writes Date32, UTC microsecond timestamps,
  and Time64 microseconds. Partial values floor to period start, date-only
  timestamps use midnight UTC, offsets normalize to UTC, and timestamps without
  offsets mean UTC. Submicrosecond precision is truncated. Invalid/non-string
  values become null with a warning on stderr, without logging their contents.
  Null input stays null without warning. Native output loses original precision
  and offset; it does not add precision columns. Accepted but ignored for other
  output formats.
- `--threads <n>` — threads for parsing/evaluation; defaults to available CPUs.
  Output row order is preserved.
- `--out <path>` — write to this file instead of stdout (a directory when
  running a directory of views).

Parquet output requires `cargo install octofhir-sof-cli --features parquet`
(or a prebuilt release binary). A binary built without the feature reports an
error for `--output parquet`; passing the temporal flag does not enable Parquet.

```sh
octofhir-sof run examples/temporal_view.json --input examples/temporal_resources.ndjson \
  --output parquet --parquet-temporal native --out temporal.parquet
```

The encoding applies to files, stdin or PostgreSQL input, and to a single view
or every view in a directory. CSV/JSON/NDJSON output is unchanged.

NDJSON-in / NDJSON-out for a single view streams resource-by-resource in bounded
memory.

### `validate`

Validate a ViewDefinition's structure against the spec, offline (no package, no
database): SQL-safe names, one iteration construct per select, unique column
names, consistent `unionAll` branches.

- `--json` — emit findings as machine-readable JSON.
- `--sarif` — emit findings as SARIF 2.1.0 (conflicts with `--json`).

### `lint`

Validate a ViewDefinition's FHIRPath selectors and generated SQL against a FHIR
package, and/or the portable ShareableViewDefinition subset.

- `--package <name>` — FHIR package (e.g. `hl7.fhir.r4.core`). Enables
  schema-driven selector linting. Optional when `--shareable` is used.
- `--version <v>` — package version. When given, the package is installed if
  missing; otherwise only an already-present package is used (offline).
- `--shareable` — enforce the ShareableViewDefinition FHIRPath subset (FH11).
- `--allow-fn <name>` — exempt a custom FHIRPath function from the `--shareable`
  allow-list (repeatable).
- `--json` / `--sarif` — machine-readable output (SARIF conflicts with `--json`).

At least one of `--package` or `--shareable` is required.

### `test`

Run a SQL-on-FHIR content-test file (or a directory of them) in memory and
report pass/fail. The format is
`{resources, tests:[{title, view, expect|expectCount|expectColumns|expectError}]}`.
Exits non-zero on any failure.
