//! Isolate subscriber capture from other tests registering tracing callsites.
#![cfg(feature = "parquet")]

use octofhir_sof::output::{OutputWriter, ParquetWriter, TemporalEncoding};
use octofhir_sof::{ColumnInfo, ColumnType, ViewResult};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

fn write(values: Vec<Value>) {
    let result = ViewResult {
        columns: vec![ColumnInfo::new("temporal", ColumnType::Date)],
        row_count: values.len(),
        data: values.into_iter().map(|v| vec![v]).collect(),
    };
    OutputWriter::write(
        &ParquetWriter::new().with_temporal(TemporalEncoding::Native),
        &result,
        &mut Vec::new(),
    )
    .unwrap();
}

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn invalid_warnings_do_not_expose_values_and_null_is_silent() {
    let logs = Logs::default();
    let output = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || output.clone())
        .with_max_level(tracing::Level::WARN)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        write(vec![Value::Null]);
        assert!(logs.0.lock().unwrap().is_empty());
        write(vec![json!("SECRET_PATIENT_VALUE"), json!(123456789)]);
    });
    let output = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert_eq!(output.matches("Invalid temporal value").count(), 2);
    assert!(output.contains("temporal"));
    assert!(output.contains("date"));
    assert!(!output.contains("SECRET_PATIENT_VALUE"));
    assert!(!output.contains("123456789"));
}
