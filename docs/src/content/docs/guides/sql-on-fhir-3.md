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

`octofhir-sof` implements the parts the specification defines in its core,
without a database or a network: the host decides where artifacts come from
and how SQL runs (the OctoFHIR server runs them on its PostgreSQL in a
sandbox). For hosts that have neither, the library also brings them, each
behind a feature:

| Piece | Feature | What it is |
|---|---|---|
| `catalog::Catalog`, `CatalogSource` | — (`packages` for `.tgz`) | dependencies from files, directories and FHIR packages |
| `terminology::HttpTerminologyProvider` (octofhir-fhir-model) | `tx-remote` | a FHIR terminology server for what the local expander cannot do — the same provider `memberOf()` in views uses |
| `embedded::DuckDbJob` | `duckdb` | jobs in an embedded DuckDB, no server |
| `guard::check` | `guard` | the static check of SQL a host runs on its own PostgreSQL |

The CLI puts them together: `octofhir-sof sql build | check | run | export`
(see [the CLI reference](/sof/reference/cli/#sql)).

## Libraries

```rust
use octofhir_sof::library::{SqlLibrary, bind_parameters};

// The SQL of the requested dialect, else the portable application/sql one.
let query = SqlLibrary::from_json(&library_json, "duckdb")?;
// Values of a Parameters resource, checked by name and declared type.
let values = bind_parameters(&query, Some(&parameters_json))?;
```

Parameters are written `:name` and always bound, never interpolated.
`library::positional_parameters` rewrites the declared ones to `$1`, `$2`, ...
cast to their declared types (lexically: other `:` uses, literals, quoted
identifiers and comments stay as written). Integer, decimal and boolean
parameters bind as numbers and booleans; `date`, `dateTime`, `instant` and
`time` bind as text in their FHIR form, as the temporal columns of a view
are (the specification's default type mapping), so the specification's own
`bp.effective_date >= :from_date` compares like with like. A value that is
not a valid FHIR date, dateTime, instant or time is refused.

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
is none) and give a ValueSet dependency's rows — typically
`catalog::value_set_content`, which takes the artifact's own expansion, else
the strict expander below, else a terminology server.

## Artifacts from files

A `catalog::Catalog` indexes ViewDefinitions, Libraries, ValueSets,
CodeSystems and ConceptMaps from JSON and NDJSON files, Bundles, directories
and FHIR packages (an extracted package directory uses its `.index.json` to
skip other files unread; a `.tgz` needs the `packages` feature). Origins are
recorded: `file views/conditions.json`, `package hl7.fhir.r4.core#4.0.1`.

```rust
use octofhir_sof::catalog::{Catalog, CatalogSource};
use octofhir_sof::terminology::HttpTerminologyProvider;

let mut catalog = Catalog::new();
catalog.load("artifacts/".as_ref())?;
catalog.load("hl7.fhir.r4.core-4.0.1.tgz".as_ref())?;   // feature `packages`

let tx = HttpTerminologyProvider::new("https://tx.fhir.org/r4".into())?; // feature `tx-remote`
let source = CatalogSource::new(&catalog).with_remote(&tx);
let mut resolver = Resolver::new(&source, &context, "duckdb");
```

A canonical without `|version` resolves to the latest version (by the
resources' `versionAlgorithm[x]`, else a natural comparison of the version
segments); two different resources with that version are ambiguous (404).
The specification leaves the choice to the runner and asks that it be
recorded: the provenance names the version used.

The terminology server is any `TerminologyProvider` of octofhir-fhir-model
— the trait the FHIRPath engine already uses — through its strict
`expand_valueset_resource` / `expand_valueset_definition`, which keep the
whole expansion (total, offset, versions, nested and inactive entries,
extensions) and tell an unknown value set from a failure. With one, a value
set the catalog lacks is asked for by canonical, and one beyond the local expander (a SNOMED CT or LOINC filter) is
expanded there. `with_remote_definitions` also sends the definition of a
value set the server does not know by canonical; the definition then leaves
the host, so it is opt-in.

## Running a job in DuckDB

`embedded::DuckDbJob` (feature `duckdb`, DuckDB bundled) runs a resolved job
with no server: each ViewDefinition is evaluated by the in-memory engine and
loaded into a typed table, each terminology relation into a table of its
columns, and the query reads every dependency under its label as a CTE, with
SQLViews as CTEs over their own dependencies.

```rust
use octofhir_sof::embedded::DuckDbJob;

let mut duck = DuckDbJob::open_in_memory()?;
duck.load(&job, &resources, threads, &CompileOptions::default())?;
duck.sandbox()?; // no file or network access for the SQL from here on
let result = duck.collect(&job, &edges, &query, &values)?; // or run(.., &mut sink)
```

`describe` gives the result columns without reading a row, so a job loaded
over no resources checks a query's SQL against its dependencies' shapes.

## Running on your own PostgreSQL

`guard::check` (feature `guard`) is the static half of running someone
else's SQL on a shared PostgreSQL: on the syntax tree, one `SELECT`, reading
only the Library's dependency labels and its own CTEs, calling only
functions from an allowlist of pure computation, unqualified; `:name`
parameters become `$n::type`. The other half is the database's: a read-only
transaction under a role with no table privileges, over temporary tables
holding the job's dependencies.

## Terminology as tables

| Relation | Columns |
|---|---|
| ValueSet | `system`, `version`, `code`, `display`, `inactive` |
| ConceptMap | `source_system`, `source_version`, `source_code`, `source_display`, `target_system`, `target_version`, `target_code`, `target_display`, `relationship` |

`relations::value_set_rows` takes an expansion (unique on system, version and
code; abstract entries excluded; nested entries flattened; a page or a partial
expansion refused). An expansion marked unclosed, as terminology servers mark
SNOMED CT expansions because post-coordinated expressions exist beyond any
list, is taken when its `total` shows every enumerable member is listed, and
the provenance records `unclosed`; without a `total` it is refused. `relations::concept_map_rows` takes a ConceptMap (R4 or
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
