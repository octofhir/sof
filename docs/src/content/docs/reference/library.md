---
title: Library (crates)
description: The octofhir-sof, octofhir-sof-lint and octofhir-sof-cli crates and their entry points.
---

Three crates make up the project:

- **`octofhir-sof`** — the network-free engine: parsing, in-memory evaluation,
  and SQL generation.
- **`octofhir-sof-lint`** — offline lint and validation of ViewDefinitions.
- **`octofhir-sof-cli`** — the thin `octofhir-sof` binary over the two above.

## `octofhir-sof`

```rust
use octofhir_sof::{ViewDefinition, execute, SqlGenerator};

let view = ViewDefinition::parse(&json_text)?;

// In-memory evaluation — returns a ViewResult (rows + column metadata):
let result = execute(&view, &resources).await?;

// Or generate SQL:
let sql = SqlGenerator::new().generate(&view)?.sql;
```

### Execution

- `execute(&view, &resources) -> Result<ViewResult>` — evaluate a view over a
  slice of `serde_json::Value` resources in memory.
- `CompiledView::compile(&view).await?` then `compiled.execute_resource(&resource).await?` —
  compile once and run resource-at-a-time for bounded-memory streaming;
  `compiled.columns()` exposes the output columns.
- `ViewRunner::new(pool).run(&view).await` — run against a PostgreSQL pool.
- `ViewResult` carries `columns`, `row_count`, `to_json_array()`, etc.

### SQL generation

- `SqlGenerator::new()` / `SqlGenerator::new().with_dialect(Dialect)` — build a
  generator; `.generate(&view)?` returns a `GeneratedSql` (`sql` plus
  `columns: Vec<GeneratedColumn>`).
- `Dialect` — `Postgres` or `DuckDb` (parses from `"postgres"` / `"duckdb"` /
  `"ansi"`).
- `create_table(name, &columns, dialect) -> String` — emit `CREATE TABLE` DDL.

### ViewDefinition model

`ViewDefinition` (with `Column`, `SelectColumn`, `Constant`, `WhereClause`)
parses from text with `ViewDefinition::parse(&str)` or from a value with
`ViewDefinition::from_json(&value)`.

### Output writers

`octofhir_sof::output::get_writer(format)` returns a writer for `csv`, `ndjson`,
`json` (and `parquet` with the feature) that serializes a `ViewResult` to any
`Write` sink.

### Native Parquet temporal columns

Enable the optional writer with:

```toml
[dependencies]
octofhir-sof = { version = "0.1.3", features = ["parquet"] }
```

```rust
use octofhir_sof::output::{OutputWriter, ParquetWriter, TemporalEncoding};

let writer = ParquetWriter::new().with_temporal(TemporalEncoding::Native);
let mut file = std::fs::File::create("result.parquet")?;
writer.write(&result, &mut file)?;
```

The same configuration works with `AsyncOutputWriter::write` and
`AsyncOutputWriter::write_streaming`. Parquet currently buffers the result;
this option does not add bounded-memory Parquet streaming.

| ColumnType | Text (default) | Native |
|---|---|---|
| Date | Utf8 | Date32 |
| DateTime, Instant | Utf8 | Timestamp(Microsecond, UTC) |
| Time | Utf8 | Time64(Microsecond) |

`new()`, `default()`, `get_writer("parquet")` and `AsyncWriter::new("parquet")`
continue to use Text. `TemporalEncoding::parse` / `FromStr` accept `text`
(`string` alias) and `native`, ignoring case and surrounding whitespace.

Native mode converts `2026` to January 1 and `2026-03` to March 1; date-only
timestamps become midnight UTC. Explicit offsets normalize to UTC; timestamps
without offsets are treated as UTC. Time values may include seconds and
fractions, or just hours and minutes. Precision finer than microseconds is
truncated. Conversion is lenient, not strict FHIR validation, including for
`instant` columns. Invalid or non-string values become null with a tracing
warning containing the column name/type but not the source value. Missing and
null values stay null silently. A non-nullable column containing invalid values
returns `Error::Output` rather than silently changing its schema.

Native output discards original precision and offsets; Text preserves the
original strings. There is no additional precision column. Other column types,
including JSON collection columns, are unaffected. SQL and ANSI type mappings
are unchanged. Embedders control their own tracing subscriber; the CLI sends
warnings to stderr.

The core library does not depend on `reqwest`, with or without Parquet. The lint
crate still includes HTTP dependencies for schema/package providers.

## `octofhir-sof-lint`

```rust
use octofhir_sof_lint::{
    lint, lint_view, lint_sql, lint_shareable, validate_structure,
    FhirSchemaProvider, Finding, Severity,
};
```

- `validate_structure(&view) -> Vec<Finding>` — structural spec checks
  (FH06–FH10), no package needed.
- `lint(&view, &provider) -> Vec<Finding>` — structural + schema-driven selector
  checks (FH01–FH05), without generated-SQL analysis.
- `lint_view` — selector-only validation. `lint_sql` explicitly opts in to
  generated-SQL analysis; it is not called by `lint`.
- `lint_shareable(&view, &allowed_custom) -> Vec<Finding>` — the
  ShareableViewDefinition FHIRPath subset check (FH11).
- `FhirSchemaProvider::load(package, version).await?` — load a FHIR package
  through the canonical manager; `with_schemas([...])` builds one from in-memory
  schemas.
- `Finding` — `{ code, message, severity, location, help_url }`; the `help_url`
  for an `FH*` rule points at its [rule reference](/sof/rules/) page.
