//! `octofhir-sof sql …` over the SQL on FHIR 3.0 example Libraries
//! (`examples/sql`): the installed interface and real output files.
#![cfg(feature = "duckdb")]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/sql")
}

fn library(name: &str) -> PathBuf {
    examples().join(format!("libraries/Library-{name}.json"))
}

fn sql(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_octofhir-sof"))
        .env_remove("SOF_TX")
        .arg("--no-color")
        .arg("sql")
        .args(args)
        .output()
        .expect("run octofhir-sof")
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).expect("utf-8")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sof-sql-cli-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_ndjson(name: &str, extra: &[&str]) -> Vec<Value> {
    let library = library(name);
    let examples = examples();
    let input = examples.join("resources.ndjson");
    let mut args = vec![
        "run",
        library.to_str().unwrap(),
        "-a",
        examples.to_str().unwrap(),
        "--input",
        input.to_str().unwrap(),
        "--output",
        "ndjson",
    ];
    args.extend_from_slice(extra);
    stdout(&sql(&args))
        .lines()
        .map(|line| serde_json::from_str(line).expect("ndjson row"))
        .collect()
}

#[test]
fn a_value_set_dependency_filters_rows() {
    let rows = run_ndjson("CardiovascularDiseasePatientsQuery", &[]);
    let mut codes: Vec<(String, String)> = rows
        .iter()
        .map(|row| {
            (
                row["patient_id"].as_str().unwrap().to_owned(),
                row["code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    codes.sort();
    assert_eq!(
        codes,
        [
            ("p1".to_owned(), "22298006".to_owned()),
            ("p2".to_owned(), "38341003".to_owned())
        ]
    );
}

#[test]
fn a_concept_map_dependency_translates_codes() {
    let rows = run_ndjson("ConditionsToIcd10Query", &[]);
    let asthma = rows.iter().find(|row| row["code"] == "195967001").unwrap();
    assert_eq!(asthma["target_code"], Value::Null);
    let infarction = rows.iter().find(|row| row["code"] == "22298006").unwrap();
    assert_eq!(infarction["target_code"], "I21.9");
    assert_eq!(rows.len(), 4);
}

#[test]
fn parameters_bind_by_declared_type() {
    let rows = run_ndjson(
        "SqlOnFhirExample",
        &[
            "-p",
            "patient_id=p1",
            "-p",
            "from_date=2026-01-01",
            "-p",
            "to_date=2026-03-31",
        ],
    );
    assert_eq!(
        rows,
        [
            json!({"patient_id": "p1", "name": "Ada Lovelace", "systolic": 142, "diastolic": 91,
                   "effective_date": "2026-01-05T09:00:00Z"}),
            json!({"patient_id": "p1", "name": "Ada Lovelace", "systolic": 128, "diastolic": 84,
                   "effective_date": "2026-02-10T09:00:00Z"}),
        ]
    );
    let refused = sql(&[
        "run",
        library("SqlOnFhirExample").to_str().unwrap(),
        "-a",
        examples().to_str().unwrap(),
        "--input",
        examples().join("resources.ndjson").to_str().unwrap(),
        "-p",
        "from_date=2026-13-01",
    ]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("is not a FHIR date"));
}

#[test]
fn a_sql_view_reads_as_a_table_and_window_functions_run() {
    let rows = run_ndjson("ActivePatientAddressesQuery", &[]);
    assert!(
        rows.iter().all(|row| row["patient_id"] != "p3"),
        "p3 is inactive"
    );
    assert_eq!(rows.len(), 3);
    let latest = run_ndjson("UniquePatientAddressesQuery", &["-p", "city=Boston"]);
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0]["address_line1"], "5 Engine Rd");
}

#[test]
fn check_reports_dependencies_and_result_columns() {
    let report: Value = serde_json::from_str(&stdout(&sql(&[
        "check",
        library("ConditionsToIcd10Query").to_str().unwrap(),
        "-a",
        examples().to_str().unwrap(),
        "--json",
    ])))
    .unwrap();
    assert_eq!(report["kind"], "SQLQuery");
    assert_eq!(report["dependencies"][1]["resourceType"], "ConceptMap");
    assert_eq!(report["dependencies"][1]["version"], "2026");
    let columns: Vec<&str> = report["columns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|column| column["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        columns,
        ["patient_id", "code", "target_code", "relationship"]
    );

    let missing = sql(&["check", library("ConditionsToIcd10Query").to_str().unwrap()]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("not found"));
}

#[test]
fn context_resources_take_precedence_and_must_be_used() {
    let dir = scratch("context");
    let pinned = dir.join("vs.json");
    fs::write(
        &pinned,
        json!({"resourceType": "ValueSet", "url": "http://example.org/ValueSet/cardiovascular-disease",
               "version": "2026", "expansion": {"contains": [
                   {"system": "http://snomed.info/sct", "code": "38341003"}]}})
        .to_string(),
    )
    .unwrap();
    let rows = run_ndjson(
        "CardiovascularDiseasePatientsQuery",
        &["--context", pinned.to_str().unwrap()],
    );
    assert_eq!(rows, [json!({"patient_id": "p2", "code": "38341003"})]);

    let unused = dir.join("unused.json");
    fs::write(
        &unused,
        json!({"resourceType": "ValueSet", "url": "http://example.org/ValueSet/other"}).to_string(),
    )
    .unwrap();
    let refused = sql(&[
        "check",
        library("CardiovascularDiseasePatientsQuery")
            .to_str()
            .unwrap(),
        "-a",
        examples().to_str().unwrap(),
        "--context",
        unused.to_str().unwrap(),
    ]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("matches no dependency"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn export_writes_one_file_per_query_and_a_manifest() {
    let dir = scratch("export");
    let examples = examples();
    let output = sql(&[
        "export",
        library("CardiovascularDiseasePatientsQuery")
            .to_str()
            .unwrap(),
        library("ConditionsToIcd10Query").to_str().unwrap(),
        "-a",
        examples.to_str().unwrap(),
        "--input",
        examples.join("resources.ndjson").to_str().unwrap(),
        "--output",
        "csv",
        "--out",
        dir.to_str().unwrap(),
    ]);
    stdout(&output);
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["output"][0]["file"],
        "CardiovascularDiseasePatientsQuery.csv"
    );
    assert_eq!(manifest["output"][0]["rows"], 2);
    assert_eq!(manifest["output"][1]["rows"], 4);
    // One job: the value set and the concept map, each resolved once.
    assert_eq!(manifest["terminology"].as_array().unwrap().len(), 2);
    let csv = fs::read_to_string(dir.join("ConditionsToIcd10Query.csv")).unwrap();
    assert!(csv.starts_with("patient_id,code,target_code,relationship\r\n"));
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn build_turns_annotated_sql_into_a_library_that_runs() {
    let dir = scratch("build");
    let source = dir.join("cvd_since.sql");
    fs::write(
        &source,
        "-- @name: CvdSince\n\
         -- @relatedDependency: https://example.org/ViewDefinition/conditions\n\
         -- @relatedDependency: http://example.org/ValueSet/cardiovascular-disease|2026 as cvd\n\
         -- @param: since date Onset on or after\n\
         SELECT conditions.patient_id, conditions.onset\n\
         FROM conditions JOIN cvd ON cvd.system = conditions.system AND cvd.code = conditions.code\n\
         WHERE conditions.onset >= :since\n",
    )
    .unwrap();
    let built: Value = serde_json::from_str(&stdout(&sql(&[
        "build",
        source.to_str().unwrap(),
        "--url",
        "https://example.org/Library/cvd-since",
    ])))
    .unwrap();
    assert_eq!(built["name"], "CvdSince");
    assert_eq!(built["relatedArtifact"][1]["label"], "cvd");
    // Annotated SQL runs as it is, too.
    let examples = examples();
    let rows: Vec<Value> = stdout(&sql(&[
        "run",
        source.to_str().unwrap(),
        "-a",
        examples.to_str().unwrap(),
        "--input",
        examples.join("resources.ndjson").to_str().unwrap(),
        "-p",
        "since=2024",
        "--output",
        "ndjson",
    ]))
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
    assert_eq!(
        rows,
        [json!({"patient_id": "p1", "onset": "2025-11-02T08:30:00Z"})]
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[cfg(feature = "parquet")]
#[test]
fn parquet_output_carries_the_result_types() {
    use parquet::file::reader::{FileReader, SerializedFileReader};

    let dir = scratch("parquet");
    let file = dir.join("out.parquet");
    let examples = examples();
    stdout(&sql(&[
        "run",
        library("SqlOnFhirExample").to_str().unwrap(),
        "-a",
        examples.to_str().unwrap(),
        "--input",
        examples.join("resources.ndjson").to_str().unwrap(),
        "-p",
        "patient_id=p1",
        "-p",
        "from_date=2026-01-01",
        "-p",
        "to_date=2026-12-31",
        "--output",
        "parquet",
        "--out",
        file.to_str().unwrap(),
    ]));
    let reader = SerializedFileReader::new(fs::File::open(&file).unwrap()).unwrap();
    assert_eq!(reader.metadata().file_metadata().num_rows(), 3);
    let schema = reader.metadata().file_metadata().schema_descr();
    assert_eq!(schema.column(2).name(), "systolic");
    fs::remove_dir_all(&dir).unwrap();
}

/// A SNOMED CT `is-a` value set is beyond the local expander; the
/// terminology server expands its definition (network: `--ignored`).
#[cfg(feature = "tx-remote")]
#[test]
#[ignore = "needs https://tx.fhir.org"]
fn a_snomed_value_set_expands_on_a_terminology_server() {
    let dir = scratch("tx");
    let value_set = dir.join("vs.json");
    fs::write(
        &value_set,
        json!({"resourceType": "ValueSet", "url": "http://example.org/ValueSet/cardiovascular-disease",
               "version": "2026", "status": "active", "compose": {"include": [{
                   "system": "http://snomed.info/sct",
                   "filter": [{"property": "concept", "op": "is-a", "value": "22298006"}]}]}})
        .to_string(),
    )
    .unwrap();
    let provenance = dir.join("provenance.json");
    let examples = examples();
    let rows: Vec<Value> = stdout(&sql(&[
        "run",
        library("CardiovascularDiseasePatientsQuery")
            .to_str()
            .unwrap(),
        "-a",
        examples.join("views").to_str().unwrap(),
        "--context",
        value_set.to_str().unwrap(),
        "--tx",
        "https://tx.fhir.org/r4",
        "--tx-definitions",
        "--input",
        examples.join("resources.ndjson").to_str().unwrap(),
        "--output",
        "ndjson",
        "--provenance",
        provenance.to_str().unwrap(),
    ]))
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect();
    assert_eq!(rows, [json!({"patient_id": "p1", "code": "22298006"})]);
    let records: Value = serde_json::from_str(&fs::read_to_string(&provenance).unwrap()).unwrap();
    assert_eq!(
        records[0]["expandedBy"],
        "terminology server https://tx.fhir.org/r4"
    );
    assert!(records[0]["members"].as_u64().unwrap() > 1);
    fs::remove_dir_all(&dir).unwrap();
}
