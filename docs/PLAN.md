# octofhir/sof — Development Plan

SQL-on-FHIR for Rust: a ViewDefinition **runtime** (parse → generate SQL → execute →
export) plus a CLI, with a static **lint/validate** layer that competitors lack.

Extracted from `server-rs/crates/octofhir-sof` (history preserved via
`git subtree split`). server-rs will repoint its dependency once this repo
publishes versioned crates.

## Positioning

Direct competitor: **Helios `hfs`** (sof-cli / sof-server / pysof, multi-DB,
multi-format). Do **not** try to out-breadth them on databases early.

Win on:
1. **Lint & validate ViewDefinitions statically** — Helios does not. `sof lint`
   checks FHIRPath column selectors against FHIR schema and lints the generated
   SQL. This is the differentiator and reuses the `mold` engine.
2. **Postgres-native correctness** — real PG grammar + JSONB via mold, not a
   generic SQL emitter.
3. **Ecosystem integration** — canonical-manager packages, fhirschema shapes,
   fhirpath evaluation, all already in-house.
4. **Offline-first** for `generate`/`lint` (no DB needed; the package is the
   schema). Live DB only for `run`.

## Target workspace

```
octofhir/sof
├── octofhir-sof            existing lib (ViewDefinition, SqlGenerator, ViewRunner, output/*)
├── octofhir-sof-lint  NEW  glue: FHIR schema → mold provider + FH lint pack
│     ├── FhirSchemaProvider  : octofhir_fhirschema::FhirSchema → mold_hir::SchemaProvider
│     └── FhirLintPack         : impl mold_hir::LintRulePack (FH01..FH05)
└── octofhir-sof-cli   NEW  binary name `octofhir-sof`
```

`octofhir-sof-cli` crate produces `[[bin]] name = "octofhir-sof"` (the lib crate
keeps the `octofhir-sof` package name; only the bin is renamed to avoid a clash).

## Dependency wiring

External (path deps first, swap to published versions later):
- `canonical-manager` — load FHIR packages by canonical / resolve StructureDefinitions.
- `octofhir-fhirschema` — StructureDefinition → FhirSchema (fields, cardinality,
  `value[x]` choice). Confirmed: it exposes everything needed.
- `octofhir-fhirpath` — already a dep of octofhir-sof; evaluate selectors.
- `mold_hir` / `mold_parser` / `mold_format` — analysis & formatting engine.

## CLI surface (`octofhir-sof`)

```
octofhir-sof generate <view.json>                       # ViewDefinition → SQL, offline, no DB
octofhir-sof run <view.json> --db $URL --output csv     # execute via ViewRunner → CSV/NDJSON/Parquet
octofhir-sof transform <view.json> --input data.ndjson  # batch transform (Helios parity)
octofhir-sof lint <view.json> --package hl7.fhir.r4.core # ★ validate ViewDefinition + lint generated SQL
```

`generate`/`lint` require only a FHIR package (offline). `run` needs a live
Postgres. Outputs reuse `octofhir-sof::output` (CSV/NDJSON/Parquet).

## Lint layer detail (the moat)

`octofhir-sof-lint`:
- `FhirSchemaProvider`: adapt `octofhir_fhirschema::FhirSchema` to
  `mold_hir::SchemaProvider`. Resource type → columns/types/cardinality. Thin
  adapter (fhirschema already gives the shape).
- `FhirLintPack` (via `mold_hir::LintRulePack`, injected through
  `AnalysisOptions.external_lint_packs`):
  - `FH01` JSONB path not in StructureDefinition (`resource->>'gendr'` → did-you-mean `gender`).
  - `FH02` choice type `value[x]` extracted without narrowing (`valueQuantity`…).
  - `FH03` `->` vs `->>` for FHIR primitives.
  - `FH04` cardinality: array field accessed without index/unnest.
  - `FH05` reference field compared without `Type/id` shape.
- `sof lint` flow: parse ViewDefinition → validate FHIRPath selectors against the
  schema → generate SQL → run it through mold (FH + MG migration + general packs).

## Milestones

**M0 — make it build standalone.** Add a root workspace `Cargo.toml`; resolve
`version.workspace`/`edition.workspace` and the `octofhir-fhirpath` dep (path to
`../fhirpath-rs` for now). Green `cargo build`/`test`. CI (fmt+clippy+test).

**M1 — CLI skeleton.** `octofhir-sof-cli` with `generate` and `run` wired to the
existing `SqlGenerator`/`ViewRunner`. Output flags reuse `output/*`.

**M2 — schema source.** `octofhir-sof-lint::FhirSchemaProvider` from fhirschema +
canonical-manager package loading. `--package` resolution.

**M3 — lint pack + `sof lint`.** FH01..FH05 + ViewDefinition selector validation.
The differentiator ships here.

**M4 — distribution.** Versioned releases, install one-liner, README with a
"vs Helios" table, announce in FHIR Zulip `#SQL-on-FHIR`.

**M5 — parity polish.** `transform` batch mode, more input formats (Bundle/NDJSON),
Parquet by default behind a feature.

## Blockers owned by the `mold` repo (must land first)

These are tracked in mold's own finalization plan and gate M2/M3:
1. Publish mold engine crates to crates.io (syntax→lexer→parser→hir→schema→format).
2. Declare `LintRulePack`, `AnalysisOptions`, `SchemaProvider`, and the completion
   provider traits as the committed stable public surface (so the FH pack doesn't
   break on minor bumps). Seams already public at
   `mold_hir/src/analyze.rs:446` (LintRulePack) and `:551` (SchemaProvider).

Until then, depend on mold via path/git from `../mold`.

## Open items

- server-rs: switch its `octofhir-sof` path dep to this repo once M0/M1 stabilize.
- Decide column→resourceType mapping convention for the generic case (Aidbox
  preset vs explicit config) — needed by `lint` outside a known server.
- `pysof`-style bindings? defer; evaluate after M3.
