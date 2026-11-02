---
title: SQL on FHIR 3.0 jobs
description: SQLQuery and SQLView libraries, the dependency graph of $sql-run and $sql-export, and terminology as tables.
sidebar:
  order: 1
---

SQL on FHIR 3.0 adds a SQL layer over views: a **SQLQuery** (a `Library`)
holds one query over its dependencies — ViewDefinitions, **SQLViews** (queries
other queries read as tables), **ValueSets** and **ConceptMaps** — named in
`relatedArtifact` with the label the SQL uses. `$sql-run` returns the rows of
one subject, `$sql-export` exports several as one job against one snapshot.

`octofhir-sof` implements the parts the specification defines, without a
database, a network or a clock: the host decides where artifacts come from and
how SQL runs (the OctoFHIR server runs them on its PostgreSQL in a sandbox).

## Libraries

```rust
use octofhir_sof::library::{SqlLibrary, bind_parameters};

// The SQL of the requested dialect, else the portable application/sql one.
let query = SqlLibrary::from_json(&library_json, "duckdb")?;
// Values of a Parameters resource, checked by name and declared type.
let values = bind_parameters(&query, Some(&parameters_json))?;
```

Libraries can be built from SQL files carrying annotations in comments
(`@name`, `@title`, `@version`, `@status`, `@param: name type [description]`,
`@relatedDependency: URL [as label]`):

```rust
use octofhir_sof::library::{AnnotatedOptions, build_annotated};

let built = build_annotated(&sql_text, &AnnotatedOptions {
    name: Some("CardiacPatients".into()),
    ..Default::default()
})?;
// built.library: a SQLQuery Library (content.data, sql-text extension);
// built.warnings: annotations it did not recognize.
```

## The dependency graph

`plan::Resolver` walks every subject's dependencies as Common Operation
Behavior specifies: one resolution per canonical, `context` entries first
(an entry without a url, two sharing one, or one matching nothing is refused),
SQLViews transitively with cycle and depth limits, a label naming two
artifacts in one query refused. Value sets and concept maps become their
relation's rows right away, so content that cannot be determined fails before
any SQL runs.

```rust
use octofhir_sof::plan::{ArtifactSource, ContextArtifacts, Resolver, closure};

let context = ContextArtifacts::from_parameters(&request_json)?;
let mut resolver = Resolver::new(&my_source, &context, "postgresql");
let edges = resolver.subject(&query).await?;
let job = resolver.finish()?;            // serde: store it with an async job
for (label, artifact) in closure(&job, &edges)? {
    // tables first, then SQLViews after what they read
}
```

`ArtifactSource` is two methods: resolve a canonical (404 semantics when there
is none) and give a ValueSet dependency's rows — typically with the strict
expander below, falling back to a terminology server.

## Terminology as tables

| Relation | Columns |
|---|---|
| ValueSet | `system`, `version`, `code`, `display`, `inactive` |
| ConceptMap | `source_system`, `source_version`, `source_code`, `source_display`, `target_system`, `target_version`, `target_code`, `target_display`, `relationship` |

`relations::value_set_rows` takes an expansion (unique on system, version and
code; abstract entries excluded; nested entries flattened; a page or a partial
expansion refused). `relations::concept_map_rows` takes a ConceptMap (R4 or
later; `noMap` rows kept with a null target; `dependsOn`, `product`,
`valueSet` and source-less groups refused; `unmapped` never applied).

`expansion::Expander` computes a membership from `compose`: enumerated codes,
whole complete code systems, hierarchy and property filters, imports
intersected with the system part, excludes. What it cannot evaluate is an
error naming the reason, never a silently smaller value set.

## Streaming output

`output::RowEncoder` encodes rows (JSON object text, as PostgreSQL's
`row_to_json` or DuckDB's `to_json` give them) into ndjson, csv or json bytes;
`output::ParquetStreamEncoder` buffers row groups and hands back the bytes of
each. Neither does I/O, so a host writes files, object storage or HTTP
bodies as it needs.
