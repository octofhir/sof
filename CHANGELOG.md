# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [0.1.0]

First release. A pure-Rust [SQL-on-FHIR v2](https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/)
toolkit: a network-free library plus a thin CLI.

### Library (`octofhir-sof`)

- **In-memory evaluator** — `execute(view, &[Value])` and a streaming
  `CompiledView`, evaluating ViewDefinitions directly over `serde_json` with no
  database.
- **Multi-dialect SQL generation** — `SqlGenerator` emits PostgreSQL (JSONB) or,
  with `with_dialect(Dialect::DuckDb)`, DuckDB (JSON) for the SELECT query, plus
  ANSI/PostgreSQL/DuckDB `CREATE TABLE` DDL.
- Full FHIRPath collection model: `forEach`/`forEachOrNull`, nested `select`,
  `unionAll`, `where`, `constant`, `%rowIndex`, `repeat`, `ofType`, boundary
  functions, and `getResourceKey`/`getReferenceKey` — including resolving
  **contained resources** (a `#id` reference keys into the resource's
  `contained[]`).
- **Version-agnostic execution** across FHIR R4/R4B/R5/R6 (navigates JSON, no
  StructureDefinition coupling).

### Lint (`octofhir-sof-lint`)

- Structural rules FH06–FH10 (SQL-safe names, one iteration construct per
  select, duplicate columns, `unionAll` shape, empty view).
- Schema-driven selector rules FH01–FH05 against a FHIR package (unknown
  element with did-you-mean, un-narrowed choice, complex/array/Reference into a
  scalar column).
- **FH11** — the ShareableViewDefinition FHIRPath allow-list (opt-in).

### CLI (`octofhir-sof`)

- `generate` (`--dialect postgres|duckdb`, `--ddl`), `run` (files/stdin/dir/`--db`,
  CSV/NDJSON/JSON/Parquet, bounded-memory streaming), `validate`, `test`, and
  `lint` (`--package`/`--version`, `--shareable`, `--allow-fn`) with
  rustc-style diagnostics, `--json` and `--sarif` output.

### Conformance

- Passes the full official v2.1 content-test suite **144/144** on both the
  in-memory and PostgreSQL paths, and on DuckDB through the `duckdb` CLI. See
  [CONFORMANCE.md](CONFORMANCE.md).

[0.1.0]: https://github.com/octofhir/sof/releases/tag/v0.1.0
