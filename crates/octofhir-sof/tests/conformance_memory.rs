//! In-memory SQL-on-FHIR v2 conformance harness.
//!
//! Runs the official `sql-on-fhir-v2` content tests through the database-free
//! [`octofhir_sof::execute`] evaluator and compares the produced rows against
//! each test's expectation. Unlike `tests/conformance.rs` this needs no
//! PostgreSQL — it runs whenever the test-case checkout is present (the sibling
//! `../fhir-test-cases/sql-on-fhir`, overridable with `SOF_TEST_CASES_DIR`).

use std::collections::BTreeSet;
use std::path::PathBuf;

use octofhir_sof::{ViewDefinition, execute};
use serde_json::Value;

#[test]
fn sql_on_fhir_conformance_in_memory() {
    let Some(dir) = test_cases_dir() else {
        eprintln!("test-cases dir not found — skipping in-memory conformance suite");
        return;
    };

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| p.file_name().is_some_and(|n| n != "manifest.json"))
        .collect();
    files.sort();

    let mut outcomes = Vec::new();
    for file in &files {
        run_file(file, &mut outcomes);
    }
    report(&outcomes);

    let failed = outcomes.iter().filter(|o| !o.passed).count();
    assert_eq!(failed, 0, "{failed} in-memory conformance cases failed");
}

struct Outcome {
    file: String,
    title: String,
    passed: bool,
    detail: String,
}

fn run_file(path: &PathBuf, outcomes: &mut Vec<Outcome>) {
    let file = path.file_name().unwrap().to_string_lossy().into_owned();
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| panic!("parse {file}: {e}"));

    let resources: Vec<Value> = doc
        .get("resources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let empty = Vec::new();
    let tests = doc.get("tests").and_then(Value::as_array).unwrap_or(&empty);
    for test in tests {
        let title = test
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("<untitled>")
            .to_string();
        outcomes.push(run_test(&file, title, test, &resources));
    }
}

fn run_test(file: &str, title: String, test: &Value, resources: &[Value]) -> Outcome {
    let expect_error = test
        .get("expectError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mk = |passed: bool, detail: String| Outcome {
        file: file.to_string(),
        title: title.clone(),
        passed,
        detail,
    };

    let view = match ViewDefinition::from_json(&test.get("view").cloned().unwrap_or(Value::Null)) {
        Ok(v) => v,
        Err(e) => return mk(expect_error, format!("parse error: {e}")),
    };

    let result = match execute(&view, resources) {
        Ok(r) => r,
        Err(e) => return mk(expect_error, format!("execute error: {e}")),
    };

    if expect_error {
        return mk(false, "expected error but execution succeeded".into());
    }

    if let Some(expect) = test.get("expect").and_then(Value::as_array) {
        let actual = result.to_json_array();
        if multiset_eq(&actual, expect) {
            mk(true, String::new())
        } else {
            mk(
                false,
                format!(
                    "rows differ: expected {} got {}",
                    Value::Array(expect.clone()),
                    Value::Array(actual)
                ),
            )
        }
    } else if let Some(count) = test.get("expectCount").and_then(Value::as_u64) {
        let got = result.row_count as u64;
        mk(got == count, format!("count: want {count} got {got}"))
    } else if let Some(cols) = test.get("expectColumns").and_then(Value::as_array) {
        let got: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        let want: Vec<&str> = cols.iter().filter_map(Value::as_str).collect();
        mk(got == want, format!("columns: want {want:?} got {got:?}"))
    } else {
        mk(true, "no expectation".into())
    }
}

fn test_cases_dir() -> Option<PathBuf> {
    if let Ok(d) = std::env::var("SOF_TEST_CASES_DIR") {
        return Some(PathBuf::from(d));
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fhir-test-cases/sql-on-fhir")
        .canonicalize()
        .ok()
}

fn multiset_eq(a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a: Vec<String> = a.iter().map(canonical).collect();
    let mut b: Vec<String> = b.iter().map(canonical).collect();
    a.sort();
    b.sort();
    a == b
}

fn canonical(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let sorted: std::collections::BTreeMap<&String, &Value> = map.iter().collect();
            let parts: Vec<String> = sorted
                .iter()
                .map(|(k, val)| format!("{k:?}:{}", canonical(val)))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

fn report(outcomes: &[Outcome]) {
    use std::collections::BTreeMap;
    let mut per_file: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for o in outcomes {
        let e = per_file.entry(o.file.as_str()).or_insert((0, 0));
        e.1 += 1;
        if o.passed {
            e.0 += 1;
        }
    }
    let (mut pass, mut total) = (0usize, 0usize);
    println!("\n=== SQL-on-FHIR conformance (in-memory) ===");
    for (file, (p, t)) in &per_file {
        println!("  {file:28} {p:>3}/{t}");
        pass += p;
        total += t;
    }
    println!("  {:-<34}", "");
    println!("  {:28} {pass:>3}/{total}", "TOTAL");

    let failures: Vec<&Outcome> = outcomes.iter().filter(|o| !o.passed).collect();
    if !failures.is_empty() {
        println!("\n--- failures ---");
        for o in &failures {
            let d: String = o.detail.chars().take(160).collect();
            println!("  [{}] {}: {}", o.file, o.title, d);
        }
    }
    let _ = BTreeSet::<()>::new();
}
