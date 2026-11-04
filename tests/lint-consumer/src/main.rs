use octofhir_sof::ViewDefinition;
use octofhir_sof_lint::{FhirSchemaProvider, lint_view};
use serde_json::json;

fn main() {
    let schema = serde_json::from_value(json!({
        "url": "http://hl7.org/fhir/StructureDefinition/Patient",
        "name": "Patient", "type": "Patient", "kind": "resource", "class": "resource",
        "elements": {"gender": {"type": "code"}, "name": {"type": "HumanName", "array": true}}
    }))
    .unwrap();
    let provider = FhirSchemaProvider::with_schemas([schema]);
    assert!(!provider.is_empty());
    for (path, expected) in [("gender", None), ("gendr", Some("FH01"))] {
        let view = ViewDefinition::from_json(&json!({
            "resource": "Patient", "select": [{"column": [{"name": "value", "path": path}]}]
        }))
        .unwrap();
        let findings = lint_view(&view, &provider);
        match expected {
            None => assert!(findings.is_empty(), "{findings:?}"),
            Some(code) => assert!(findings.iter().any(|f| f.code == code), "{findings:?}"),
        }
    }
    println!(
        "Schema-backed lint accepts valid fields and diagnoses misspellings without a package loader"
    );
}
