# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [0.1.4]

### Changed

- Updated FHIRPath to 0.4.57 and canonical manager to 0.2.4.

## [0.1.3]

### Added

- Opt-in native Parquet temporal types via
  `ParquetWriter::with_temporal(TemporalEncoding::Native)` and
  `run --output parquet --parquet-temporal native`: dates use Date32,
  dateTime/instant use UTC microsecond timestamps, and time uses Time64
  microseconds. Partial values floor to period start; invalid values become null
  with redacted warnings. Text remains the default.
- `ParquetWriter` configuration now includes `temporal`; prefer `new()` or
  `default()` with the configuration builders over struct literals.

### Changed

- Updated Rust dependencies, including FHIRPath 0.4.56 and Arrow/Parquet 60,
  and refreshed documentation dependencies to Astro 7 / Starlight 0.42.

### Fixed

- The core library no longer pulls in `reqwest` through FHIRPath/model
  dependencies, including with the Parquet feature. Schema lint providers still
  include HTTP support for FHIR packages.

## [0.1.2]

### Added

- **`SqlExecutor` trait and `run_with`** — execute a ViewDefinition by generating
  SQL and delegating row fetching to an external database layer, instead of the
  bundled sqlx `ViewRunner`. Host applications can reuse their own instrumented
  Postgres pool while keeping SoF's SQL generation and `ViewResult` shape.

### Changed

- Bumped sqlx (`sqlx-core`, `sqlx-postgres`) to `0.9` for the Postgres
  `ViewRunner` path.

## [0.1.1]

### Changed

- **In-memory execution now supports the full FHIRPath function set.** `run` and
  `execute` delegate per-expression evaluation to the `octofhir-fhirpath`
  engine, so functions beyond the SQL-on-FHIR essentials — `substring`, `upper`,
  `lower`, `length`, math, etc. — work in memory. The library stays
  network-free (no FHIR ModelProvider; `ofType` is rewritten on the path).
  SoF semantics (forEach/unionAll/repeat, `%rowIndex`, scalar-vs-collection,
  contained-resource keys) are preserved.
- The SQL generation path (`generate`, PostgreSQL/DuckDB) remains a hand-lowered
  subset and is unchanged: `run` is now intentionally richer than `generate`,
  which errors clearly on a function it cannot lower to SQL.
- `execute` and `CompiledView::{compile, execute_resource}` are now `async`; a
  sync `execute_blocking` is provided for non-async callers.

### Fixed

- `lint` no longer runs the generated SQL through the bundled SQL analyzer,
  which produced false positives (Rf01/Jb01) against the generator's
  lateral-join aliases. Findings are reported at the spec level only.

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

[0.1.4]: https://github.com/octofhir/sof/releases/tag/v0.1.4
[0.1.3]: https://github.com/octofhir/sof/releases/tag/v0.1.3
[0.1.2]: https://github.com/octofhir/sof/releases/tag/v0.1.2
[0.1.1]: https://github.com/octofhir/sof/releases/tag/v0.1.1
[0.1.0]: https://github.com/octofhir/sof/releases/tag/v0.1.0
