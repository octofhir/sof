//! Terminology functions in the in-memory evaluator.
//!
//! `memberOf()` is outside the ViewDefinition FHIRPath subset, so it is an
//! extension of this runner, and it resolves only through a
//! `TerminologyProvider` supplied in `CompileOptions`. Without one, a view that
//! calls it fails with an error that says how to supply it.

use std::sync::Arc;

use async_trait::async_trait;
use octofhir_sof::terminology::{
    ConnectionStatus, ExpansionParameters, LookupResult, SubsumptionOutcome, SubsumptionResult,
    TranslationResult, ValidationResult, ValueSetExpansion,
};
use octofhir_sof::{CompileOptions, TerminologyProvider, ViewDefinition, execute, execute_with};
use serde_json::{Value, json};

const VITALS: &str = "http://example.org/ValueSet/vitals";
const LOINC: &str = "http://loinc.org";

/// One value set with one member, LOINC 8867-4 (heart rate).
#[derive(Debug)]
struct Vitals;

#[async_trait]
impl TerminologyProvider for Vitals {
    async fn validate_code(
        &self,
        _code: &str,
        _system: &str,
        _version: Option<&str>,
    ) -> octofhir_fhir_model::Result<bool> {
        Ok(false)
    }

    async fn expand_valueset(
        &self,
        _valueset_url: &str,
        _parameters: Option<&ExpansionParameters>,
    ) -> octofhir_fhir_model::Result<ValueSetExpansion> {
        Ok(ValueSetExpansion {
            contains: Vec::new(),
            total: Some(0),
            parameters: Vec::new(),
            timestamp: None,
        })
    }

    async fn translate_code(
        &self,
        _source_code: &str,
        _target_system: &str,
        _concept_map_url: Option<&str>,
    ) -> octofhir_fhir_model::Result<TranslationResult> {
        Ok(TranslationResult {
            success: false,
            targets: Vec::new(),
            message: None,
        })
    }

    async fn lookup_code(
        &self,
        _system: &str,
        _code: &str,
        _version: Option<&str>,
        _properties: Option<Vec<&str>>,
    ) -> octofhir_fhir_model::Result<LookupResult> {
        Ok(LookupResult {
            display: None,
            definition: None,
            properties: Vec::new(),
        })
    }

    async fn validate_code_vs(
        &self,
        valueset: &str,
        system: Option<&str>,
        code: &str,
        _display: Option<&str>,
    ) -> octofhir_fhir_model::Result<ValidationResult> {
        Ok(ValidationResult {
            result: valueset == VITALS && system.is_none_or(|s| s == LOINC) && code == "8867-4",
            display: None,
            message: None,
        })
    }

    async fn subsumes(
        &self,
        _system: &str,
        _parent: &str,
        _child: &str,
    ) -> octofhir_fhir_model::Result<SubsumptionResult> {
        Ok(SubsumptionResult {
            outcome: SubsumptionOutcome::NotSubsumed,
        })
    }

    async fn test_connection(&self) -> octofhir_fhir_model::Result<ConnectionStatus> {
        Ok(ConnectionStatus {
            connected: true,
            response_time_ms: None,
            server_version: None,
            error: None,
        })
    }
}

fn view() -> ViewDefinition {
    ViewDefinition::from_json(&json!({
        "resourceType": "ViewDefinition",
        "resource": "Observation",
        "status": "active",
        "select": [{ "column": [
            { "name": "id", "path": "id" },
            { "name": "vital", "path": format!("code.memberOf('{VITALS}')"), "type": "boolean" }
        ] }]
    }))
    .expect("parse ViewDefinition")
}

fn observation(id: &str, code: Option<&str>) -> Value {
    let mut obs = json!({ "resourceType": "Observation", "id": id, "status": "final" });
    if let Some(code) = code {
        obs["code"] = json!({ "coding": [{ "system": LOINC, "code": code }] });
    }
    obs
}

#[tokio::test]
async fn member_of_resolves_through_the_supplied_provider() {
    let resources = vec![
        observation("hr", Some("8867-4")),
        observation("glucose", Some("2345-7")),
        observation("no-code", None),
    ];
    let options = CompileOptions::with_terminology(Arc::new(Vitals));
    let result = execute_with(&view(), &resources, &options)
        .await
        .expect("execute");
    let rows: Vec<(Value, Value)> = result
        .data
        .iter()
        .map(|row| (row[0].clone(), row[1].clone()))
        .collect();
    assert_eq!(
        rows,
        vec![
            (json!("hr"), json!(true)),
            (json!("glucose"), json!(false)),
            // No code: memberOf on an empty input is empty, a null column.
            (json!("no-code"), Value::Null),
        ]
    );
}

#[tokio::test]
async fn member_of_without_a_provider_says_how_to_supply_one() {
    let resources = vec![observation("hr", Some("8867-4"))];
    let error = execute(&view(), &resources)
        .await
        .expect_err("memberOf needs a provider");
    let message = error.to_string();
    assert!(
        message.contains("terminology provider") && message.contains("CompileOptions"),
        "unexpected error: {message}"
    );
}
