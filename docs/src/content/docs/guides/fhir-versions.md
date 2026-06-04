---
title: FHIR versions
description: Version-agnostic execution across R4/R4B/R5/R6, and schema-driven lint by package.
sidebar:
  order: 3
---

## Execution is version-agnostic

The execution engine navigates the resource JSON directly and is **not** coupled
to any `StructureDefinition`, so the same `ViewDefinition` runs unchanged over
R4, R4B, R5 or R6 resources. There is no FHIR version to configure on `run`,
`execute` or `generate`. The conformance suite includes an R5 `CodeableReference`
case to keep this honest.

```sh
# The same view, the same command, against R4 or R5 data:
octofhir-sof run view.json --input r4-patients.ndjson
octofhir-sof run view.json --input r5-patients.ndjson
```

## Schema-driven lint accepts any package

Only the schema-driven selector lint (FH01–FH05) needs a version, because it
checks paths against a concrete `StructureDefinition`. Pass the package name and
optionally `--version`; there is no hard-coded version:

```sh
octofhir-sof lint view.json --package hl7.fhir.r4.core
octofhir-sof lint view.json --package hl7.fhir.r4b.core
octofhir-sof lint view.json --package hl7.fhir.r5.core --version 5.0.0
```

When `--version` is given, the package is installed through the canonical
manager if it is missing; without it, only an already-present package is used
(fully offline). Any package the canonical manager can resolve works — including
an R6 build. See the [linting guide](/sof/guides/linting/).
