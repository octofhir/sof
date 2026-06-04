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
let result = execute(&view, &resources)?;

// Or generate SQL:
let sql = SqlGenerator::new().generate(&view)?.sql;
```

### Execution

- `execute(&view, &resources) -> Result<ViewResult>` — evaluate a view over a
  slice of `serde_json::Value` resources in memory.
- `CompiledView::compile(&view)?` then `compiled.execute_resource(&resource)?` —
  compile once and run resource-at-a-time for bounded-memory streaming;
  `compiled.columns()` exposes the output columns.
- `ViewRunner::new(pool).run(&view).await` — run against a PostgreSQL pool.
- `ViewResult` carries `columns`, `row_count`, `to_json_array()`, etc.

### SQL generation

- `SqlGenerator::new()` / `SqlGenerator::with_dialect(Dialect)` — build a
  generator; `.generate(&view)?` returns a `GeneratedSql` (`sql` plus
  `columns: Vec<GeneratedColumn>`).
- `Dialect` — `Postgres` or `Duckdb` (parses from `"postgres"` / `"duckdb"` /
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
  checks (FH01–FH05) + generated-SQL analysis.
- `lint_view` / `lint_sql` — the selector-only and generated-SQL-only halves.
- `lint_shareable(&view, &allowed_custom) -> Vec<Finding>` — the
  ShareableViewDefinition FHIRPath subset check (FH11).
- `FhirSchemaProvider::load(package, version).await?` — load a FHIR package
  through the canonical manager; `with_schemas([...])` builds one from in-memory
  schemas.
- `Finding` — `{ code, message, severity, location, help_url }`; the `help_url`
  for an `FH*` rule points at its [rule reference](/sof/rules/) page.
