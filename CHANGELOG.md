## [0.3.5] - 2026-11-06

### Added
- `Expander`: `compose.lockedDate` picks the versions of what is not tied to one, through the new `TerminologySource::find_as_of` and `Catalog::find_as_of`. It reaches into imported value sets, and a version named outright has to be dated by then too. A source that holds only its latest version refuses when that one is newer.
- `Expander`: the supplements named by the `valueset-supplement` extension add their properties and designations to their code system; one that cannot be found ends the expansion.
- `Expander`: a code system with `caseSensitive = false` matches codes in any case, and a member carries the code as the code system writes it.

## [0.3.4] - 2026-11-04

### Changed
- Add a lint mode without package loading (`default-features = false`): supplied schemas work without canonical-manager, SQLite or HTTP. Default package-loading helpers remain available.
- Disable the unused canonical-manager CLI and FHIRSchema HTTP client in the lint library.
- Depend directly on Arrow arrays and schemas for Parquet output instead of the full Arrow facade.
- Select the supported Parquet compression codecs explicitly; retain Snappy, Gzip, LZ4 and Zstd, including the existing zlib-rs and SIMD UTF-8 paths.
- Trim unused Tokio and futures utility features and update private base64 to 0.23.
- Refresh compatible dependencies and the minimum versions of the optimized OctoFHIR libraries.

# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/), and the project adheres to
[Semantic Versioning](https://semver.org/).

## [0.3.3] — 2026-11-03

### Added

- `catalog`: dependencies from files — `Catalog` indexes ViewDefinitions,
  Libraries, ValueSets, CodeSystems and ConceptMaps from JSON / NDJSON files,
  Bundles, directories and FHIR packages (`.index.json` skips other files;
  `.tgz` with the new `packages` feature), recording each origin; a
  versionless canonical takes the latest version (`compare_versions`:
  `versionAlgorithm[x]`, else natural order, pre-releases first), two
  different resources with it are ambiguous. `CatalogSource` is the job's
  `ArtifactSource` over it; `value_set_content` is the rule hosts share (the
  artifact's expansion, else the strict expander, else a terminology server).
- A terminology server for SQL jobs is the ecosystem's `TerminologyProvider`
  (octofhir-fhir-model 0.1.18, `HttpTerminologyProvider` with `tx-remote`),
  the same provider `memberOf()` in views uses: value sets a host lacks, or
  cannot expand, expanded by canonical through the strict
  `expand_valueset_resource`; `with_remote_definitions` also sends a
  definition the server does not know (opt-in).
- `embedded::DuckDbJob` (feature `duckdb`, bundled DuckDB 1.5): whole
  SQLQuery jobs with no server — views evaluated in memory into typed tables,
  terminology relations, SQLViews as CTEs, parameters bound positionally; a
  sandbox (`enable_external_access = false`, locked configuration);
  `describe` checks a query without data.
- `guard` (feature `guard`): the static check of SQL run on a shared
  PostgreSQL (one `SELECT` over the declared labels and its CTEs, allowlisted
  functions, `:name` → `$n::type`), moved here from the OctoFHIR server.
- `library::positional_parameters`, `DeclaredParameter::sql_type_in`,
  `ContextArtifacts::from_resources`, `ExpansionStamp::now`,
  `ColumnType::fhir_type`.
- CLI `octofhir-sof sql build | check | run | export`, with `--artifacts`,
  `--context`, `--param`, `--parameters`, `--tx`, `--tx-definitions`,
  `--provenance`; `examples/sql` runs the libraries of the 3.0 guide.

### Changed

- Temporal parameters (`date`, `dateTime`, `instant`, `time`) bind as text in
  their FHIR form, validated, as the temporal columns of a view are, so the
  guide's `bp.effective_date >= :from_date` compares like with like (it was a
  type error on PostgreSQL and DuckDB).
- An expansion marked `valueset-unclosed` (as terminology servers mark SNOMED
  CT expansions) is taken when its `total` shows every member listed, and the
  provenance records `unclosed`; Terminology in SQL defines incomplete as an
  `offset` or a `total` above the entries.
- The library needs only tokio's `rt` and `io-util` (with octofhir-fhirpath
  0.5.3, which needs only `sync`).
- Dependencies: octofhir-fhirpath 0.5.3, octofhir-fhir-model 0.1.18.

## [0.3.2] — 2026-11-03

### Changed

- Dependencies: octofhir-fhirpath 0.5.2, octofhir-fhirschema 0.3.34,
  octofhir-canonical-manager 0.2.8 (a pinned `url|version` resolves to that
  version).

## [0.3.1] — 2026-11-03

### Added

- `library::build_annotated`: a SQLQuery / SQLView Library from SQL carrying
  annotations in its comments (`@name`, `@title`, `@description`, `@version`,
  `@status`, `@author`, `@publisher`, `@param`, `@relatedDependency`), with the
  `sql-text` extension, checked by reading it back; unrecognized annotations
  are reported.
- `ViewDefinition::validate`: the structural rules of 3.0 — SQL names for the
  view, constants and columns (`sql-name`), at most one of `forEach`,
  `forEachOrNull` and `repeat` per select (`sql-expressions`), `unionAll`
  branches with the same columns and declared types. Compiling and SQL
  generation check them; parsing still accepts any view, so a linter can
  report on it.
- `output::ParquetStreamEncoder` (feature `parquet`): a Parquet file encoded
  row by row in row groups, handing back the bytes to write, for streamed
  exports.
- `Dialect::value_type`: the column type generated SQL gives a view column, so
  a host materializing rows computed elsewhere declares the same types (a
  partial date stays text).
- `expansion::Expander::with_designations`.
- The examples of the 3.0 guide are tests: every Library reads in both
  dialects, every ViewDefinition validates, compiles and lowers to SQL.

### Changed

- Documentation, README and banner describe SQL on FHIR 3.0
  (`hl7.fhir.uv.sql-on-fhir`); links point at the HL7 build. The vendored
  content tests are refreshed from FHIR/sql-on-fhir.js.

## [0.3.0]

### Added

What a SQL on FHIR v3 runner needs beyond views, as host-independent modules
(no database, network or clock): a server, the CLI or an embedded DuckDB runner
plug in only where artifacts come from and how SQL runs.

- `library`: SQLQuery / SQLView Libraries: the SQL of a requested dialect else
  the portable one, `relatedArtifact` dependencies, declared parameters, and
  `bind_parameters` (by name and declared type, missing required refused).
- `plan`: the dependency graph of a `$sql-run` / `$sql-export` job as Common
  Operation Behavior specifies it: one resolution per canonical, `context`
  entries first (no url, two sharing a url, or one matching nothing refused),
  SQLViews resolved transitively with cycle and depth limits, a label naming two
  artifacts refused, CTEs in dependency order (`closure`). The host supplies
  artifacts through `ArtifactSource`; the result (`ResolvedJob`) is serde data an
  async job can store.
- `relations`: Terminology in SQL (trial use). ValueSet relation `system,
  version, code, display, inactive` (unique on the first three, abstract entries
  excluded, nested entries flattened, partial expansions refused); ConceptMap
  relation of nine columns (noMap rows kept, `dependsOn` / `product` /
  `valueSet` / source-less groups refused, `unmapped` never applied, R4 maps
  converted); provenance records.
- `expansion`: a strict ValueSet expander for those relations: the ValueSet's
  own complete expansion, else `compose` (enumerated codes, whole complete code
  systems, hierarchy and property filters, imports intersected with the system
  part, excludes, `compose.inactive`), naming anything it cannot determine
  instead of leaving it out; optional designations. Lookups go through the
  host's `TerminologySource`; identifier and timestamp come from the host.
- `output::RowEncoder` / `RowFormat`: row-at-a-time ndjson, csv and json for
  streamed exports (JSON object text in column order passes through).
- `Error::InvalidLibrary`, `Error::InvalidParameter`.

## [0.2.1]

### Security

- Patched dependencies: zstd 0.14.3, zstd-safe 8.2, zstd-sys 2.1.2, rustls-webpki
  0.103.16, webpki-roots / webpki-root-certs 1.0.11, ctutils 0.4.7 (`cargo audit`
  is clean). The documentation site moves to Astro 7 / Starlight 0.42.

## [0.2.0]

### Added

- `CompileOptions` with a `TerminologyProvider` for the in-memory engine:
  `memberOf()`, `subsumes()` and the other terminology functions run in views.
  The CLI takes `--tx <url>` / `SOF_TX` (feature `tx-remote`).
- `SqlGenerator::with_source(relation)`: a view reads a named relation (for
  example an authorized CTE) instead of the resource table.
- SQL lowering beyond the required subset: `upper`, `lower`, `length`, `trim`,
  `startsWith`, `endsWith`, `contains`, `indexOf`, `substring`, `replace`, `iif`,
  `select`, `all`, `skip`, `take`, `tail`, `distinct`, `abs` (integer),
  `ceiling` / `floor` / `truncate` (typed), and `~` per FHIRPath string
  equivalence. Each runs the same on the in-memory engine, PostgreSQL and DuckDB.

### Changed

- Database backends and outputs are opt-in features: `postgres` (`ViewRunner`),
  `parquet`, `tx-remote`. The core library has no database driver (breaking:
  enable `postgres` for `ViewRunner`).
- Columns without a declared type keep their JSON type on every engine; rows
  keep the view's column order in json and ndjson.
- FHIRPath 0.5.1, UCUM 0.9.1, FHIRSchema 0.3.33.

### Fixed

- Text from a view (a `join()` separator, a string literal, an extension URL)
  is always quoted as a SQL literal; a quote in it could end the literal.

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
