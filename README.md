# octofhir/sof

A Rust toolkit for [SQL on FHIR v2](https://build.fhir.org/ig/FHIR/sql-on-fhir-v2/):
parse `ViewDefinition` resources and turn FHIR data into flat tables — with no
database required, or by generating PostgreSQL.

- **Two interchangeable execution paths**, both passing the full official
  content-test suite (144/144):
  - a **database-free in-memory evaluator** over `serde_json`, and
  - a **PostgreSQL generator** that lowers FHIRPath to SQL over JSONB.
- **Writers**: CSV, NDJSON, JSON, and Parquet (feature-gated).
- **Offline lint & validation** of ViewDefinitions (FH01–FH10) with rustc-style
  diagnostics, plus JSON and SARIF output for CI.
- Full support for the v2.1 additions: the `repeat` directive and the
  `%rowIndex` environment variable, on both paths.

## Install

```sh
# From crates.io:
cargo install octofhir-sof-cli

# …or from a clone of this repo:
cargo install --path crates/octofhir-sof-cli

# …or build and run in place:
cargo build --release
./target/release/octofhir-sof --help
```

Prebuilt binaries for macOS, Linux and Windows are attached to each
[GitHub release](https://github.com/octofhir/sof/releases).

## Usage

```sh
# Run a view over FHIR files, no database — CSV to stdout
octofhir-sof run view.json --input patients.ndjson

# Stream a large NDJSON export with bounded memory (NDJSON in, NDJSON out)
cat export/*.ndjson | octofhir-sof run view.json --input - --output ndjson

# Run every view in a directory in one pass, one output file per view
octofhir-sof run views/ --input data/ --output csv --out out/

# Generate PostgreSQL for the view
octofhir-sof generate view.json

# Generate a CREATE TABLE for the view's columns (ansi | postgres | duckdb)
octofhir-sof generate view.json --ddl --dialect postgres

# Run against PostgreSQL instead of files
octofhir-sof run view.json --db postgres://localhost/fhir --output csv

# Validate a ViewDefinition offline (no FHIR package needed)
octofhir-sof validate view.json
octofhir-sof validate view.json --sarif > results.sarif

# Lint selectors and generated SQL against a FHIR package
octofhir-sof lint view.json --package hl7.fhir.r4.core

# Run the official content-test format in memory
octofhir-sof test tests/
```

Exit codes: `0` on success, `1` on validation errors, lint errors, test
failures, or I/O errors.

## Library

```rust
use octofhir_sof::{ViewDefinition, execute, SqlGenerator};

let view = ViewDefinition::parse(&json_text)?;

// In-memory:
let result = execute(&view, &resources)?;

// Or generate SQL:
let sql = SqlGenerator::new().generate(&view)?.sql;
```

For bounded-memory streaming, compile once and run resource-at-a-time:

```rust
use octofhir_sof::CompiledView;

let compiled = CompiledView::compile(&view)?;
for resource in resources {
    for row in compiled.execute_resource(&resource)? {
        // row is the column values in column order
    }
}
```

## Conformance

The official reference tests from
[FHIR/sql-on-fhir.js](https://github.com/FHIR/sql-on-fhir.js) are vendored under
`crates/octofhir-sof/tests/spec/` (refresh with `just update-spec-tests`). Run
them:

```sh
just conformance                 # in-memory (no database)
just conformance-pg              # against PostgreSQL (needs Docker)
```

## License

MIT OR Apache-2.0.
