//! The examples of SQL on FHIR 3.0.0 (continuous build, `tests/fixtures/v3-examples`):
//! every SQLQuery / SQLView Library reads in both dialects, and every
//! ViewDefinition passes the structural invariants, compiles for the
//! in-memory engine and lowers to SQL (or names the path it cannot lower).

use std::fs;
use std::path::PathBuf;

use octofhir_sof::library::{SqlLibrary, SqlLibraryKind};
use octofhir_sof::{CompiledView, Error, SqlGenerator, ViewDefinition};
use serde_json::Value;

fn examples(prefix: &str) -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v3-examples");
    let mut found: Vec<(String, Value)> = fs::read_dir(&dir)
        .expect("fixtures")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?.to_owned();
            (name.starts_with(prefix) && name.ends_with(".json")).then(|| {
                let text = fs::read_to_string(&path).expect("read");
                (name, serde_json::from_str(&text).expect("json"))
            })
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!found.is_empty(), "no {prefix} examples");
    found
}

#[test]
fn every_example_library_reads() {
    for (name, library) in examples("Library-") {
        for dialect in ["postgresql", "duckdb"] {
            let parsed = SqlLibrary::from_json(&library, dialect)
                .unwrap_or_else(|error| panic!("{name} ({dialect}): {error}"));
            assert!(!parsed.sql.trim().is_empty(), "{name}");
            let declared = library["type"]["coding"][0]["code"].as_str();
            match declared {
                Some("sql-view") => assert_eq!(parsed.kind, SqlLibraryKind::View, "{name}"),
                _ => assert_eq!(parsed.kind, SqlLibraryKind::Query, "{name}"),
            }
        }
    }
}

#[tokio::test]
async fn every_example_view_validates_and_compiles() {
    for (name, view) in examples("ViewDefinition-") {
        let parsed =
            ViewDefinition::from_json(&view).unwrap_or_else(|error| panic!("{name}: {error}"));
        parsed
            .validate()
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        CompiledView::compile(&parsed)
            .await
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        match SqlGenerator::new().generate(&parsed) {
            Ok(_) | Err(Error::InvalidPath(_) | Error::FhirPath(_)) => {}
            Err(error) => panic!("{name}: {error}"),
        }
    }
}
