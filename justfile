# octofhir/sof — task runner. Run `just` to list recipes.

# The CLI, run from source (quiet so demo output is clean).
sof := "cargo run --quiet -p octofhir-sof-cli --"
examples := "examples"

# List available recipes.
default:
    @just --list

# --- Development ---

# Build the whole workspace.
build:
    cargo build --workspace

# Run the full test suite (includes the database-free conformance harness).
test:
    cargo test --workspace

# Format all code.
fmt:
    cargo fmt --all

# Check formatting without writing.
fmt-check:
    cargo fmt --all -- --check

# Lint with clippy, denying warnings.
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# The pre-commit gate: format, clippy and tests must all pass.
check: fmt-check clippy test

# Install the CLI into ~/.cargo/bin.
install:
    cargo install --path crates/octofhir-sof-cli

# --- Conformance ---

# Run the official content tests in memory (no database).
conformance:
    cargo test -p octofhir-sof --test conformance_memory -- --nocapture

# Run the official content tests against PostgreSQL (set SOF_CONFORMANCE_DB).
conformance-pg db="postgres://postgres:postgres@localhost:55433/conformance":
    SOF_CONFORMANCE_DB="{{db}}" cargo test -p octofhir-sof --test conformance -- --nocapture

# Start a throwaway PostgreSQL for conformance-pg.
conformance-db:
    docker run -d --name sof-conformance -e POSTGRES_PASSWORD=postgres \
      -e POSTGRES_DB=conformance -p 55433:5432 postgres:16-alpine

# --- CLI passthrough ---

# Generate PostgreSQL from a ViewDefinition.
generate view:
    {{sof}} generate {{view}}

# Run a ViewDefinition against FHIR files (no database).
run view input:
    {{sof}} run {{view}} --input {{input}}

# Validate a ViewDefinition's structure against the spec (offline).
validate view:
    {{sof}} validate {{view}}

# Run a SQL-on-FHIR test-case file (or directory) in memory.
test-cases manifest:
    {{sof}} test {{manifest}}

# --- Demo ---

# End-to-end tour of the CLI over examples/, no database required.
demo:
    @echo "\n# 1. Validate a ViewDefinition against the spec (offline, no package)"
    {{sof}} validate {{examples}}/patient_demographics.json
    @echo "\n# 2. Catch a spec violation (duplicate column name)"
    -{{sof}} validate {{examples}}/invalid_view.json
    @echo "\n# 3. Run a view on FHIR files with NO database — CSV output"
    {{sof}} run {{examples}}/patient_demographics.json --input {{examples}}/patients.ndjson --output csv
    @echo "\n# 4. Same view, JSON output (collections stay arrays)"
    {{sof}} run {{examples}}/patient_demographics.json --input {{examples}}/patients.ndjson --output json
    @echo "\n# 5. forEach: one row per telecom entry"
    {{sof}} run {{examples}}/patient_contacts.json --input {{examples}}/patients.ndjson --output csv
    @echo "\n# 6. Generate the equivalent PostgreSQL for the same view"
    {{sof}} generate {{examples}}/patient_demographics.json
    @echo ""
