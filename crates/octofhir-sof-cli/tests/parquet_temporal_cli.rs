//! Exercise the installed interface and real output files, not CLI internals.
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_octofhir-sof"));
    command.env_remove("DATABASE_URL").arg("--no-color");
    command
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn view(resource: &str) -> Value {
    json!({"resource": resource, "select": [{"column": [
        {"name": "id", "path": "id", "type": "string"},
        {"name": "day", "path": "birthDate", "type": "date"},
        {"name": "recorded", "path": "recorded", "type": "dateTime"},
        {"name": "issued", "path": "issued", "type": "instant"},
        {"name": "at", "path": "at", "type": "time"}
    ]}]})
}

fn resources(resource: &str) -> Vec<Value> {
    vec![
        json!({"resourceType": resource, "id": "one", "birthDate": "1970-01",
            "recorded": "1970-01-01T01:00:00+01:00", "issued": "1970-01-01T00:00:00.000001Z", "at": "00:01"}),
        json!({"resourceType": resource, "id": "two", "birthDate": "1970-01-02",
            "recorded": "1970-01-02", "at": "00:00:01.123456"}),
    ]
}

struct Fixture {
    root: PathBuf,
    view: PathBuf,
    input: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "sof-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = Self {
            view: root.join("view.json"),
            input: root.join("input.ndjson"),
            root,
        };
        fs::write(&fixture.view, view("Patient").to_string()).unwrap();
        fixture.write_input(resources("Patient"));
        fixture
    }

    fn write_input(&self, resources: Vec<Value>) {
        let ndjson: String = resources.into_iter().map(|r| format!("{r}\n")).collect();
        fs::write(&self.input, ndjson).unwrap();
    }

    fn run(&self, format: &str, temporal: Option<&str>) -> Output {
        let mut command = command();
        command
            .arg("run")
            .arg(&self.view)
            .arg("--input")
            .arg(&self.input)
            .args(["--output", format]);
        if let Some(temporal) = temporal {
            command.args(["--parquet-temporal", temporal]);
        }
        command.output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn help_and_invalid_option() {
    let output = command().args(["run", "--help"]).output().unwrap();
    success(&output);
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--parquet-temporal"));
    assert!(help.contains("text, native"));
    let output = Fixture::new().run("csv", Some("epoch"));
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value"));
}

#[test]
fn non_parquet_outputs_and_ndjson_fast_path_are_unchanged() {
    let fixture = Fixture::new();
    for format in ["csv", "json", "ndjson"] {
        let original = fixture.run(format, None);
        success(&original);
        let native = fixture.run(format, Some("native"));
        success(&native);
        assert_eq!(native.stdout, original.stdout, "{format}");
        assert!(!native.stdout.is_empty());
        if format == "ndjson" {
            let text = String::from_utf8(native.stdout).unwrap();
            assert_eq!(text.lines().count(), 2);
            assert_eq!(
                serde_json::from_str::<Value>(text.lines().next().unwrap()).unwrap()["day"],
                "1970-01"
            );
        }
    }
}

#[cfg(not(feature = "parquet"))]
#[test]
fn parquet_requires_opt_in_build() {
    let output = Fixture::new().run("parquet", Some("native"));
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--features parquet"));
}

#[cfg(feature = "parquet")]
mod parquet_output {
    use super::*;
    use arrow::array::{
        Array, Date32Array, StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
    };
    use arrow::datatypes::{DataType, TimeUnit};
    use arrow::record_batch::RecordBatch;
    use bytes::Bytes;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    fn read(bytes: Vec<u8>) -> RecordBatch {
        assert_eq!(&bytes[..4], b"PAR1");
        let mut reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
            .unwrap()
            .build()
            .unwrap();
        let batch = reader.next().unwrap().unwrap();
        assert!(reader.next().is_none());
        batch
    }

    fn assert_native(batch: &RecordBatch, rows: usize) {
        assert_eq!(batch.num_rows(), rows);
        assert_eq!(batch.schema().field(1).data_type(), &DataType::Date32);
        for i in [2, 3] {
            assert_eq!(
                batch.schema().field(i).data_type(),
                &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
            );
        }
        assert_eq!(
            batch.schema().field(4).data_type(),
            &DataType::Time64(TimeUnit::Microsecond)
        );
        let days = batch
            .column(1)
            .as_any()
            .downcast_ref::<Date32Array>()
            .unwrap();
        assert_eq!(days.value(0), 0);
        let recorded = batch
            .column(2)
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(recorded.value(0), 0);
        let issued = batch
            .column(3)
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(issued.value(0), 1);
        let time = batch
            .column(4)
            .as_any()
            .downcast_ref::<Time64MicrosecondArray>()
            .unwrap();
        assert_eq!(time.value(0), 60_000_000);
    }

    #[test]
    fn text_default_and_native_stdout_round_trip() {
        let fixture = Fixture::new();
        let output = fixture.run("parquet", None);
        success(&output);
        let default = read(output.stdout);
        let output = fixture.run("parquet", Some("text"));
        success(&output);
        assert_eq!(default, read(output.stdout));
        for column in default.columns() {
            assert_eq!(column.data_type(), &DataType::Utf8);
        }
        assert_eq!(
            default
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap()
                .value(0),
            "1970-01"
        );
        let output = fixture.run("parquet", Some("native"));
        success(&output);
        assert!(output.stderr.is_empty());
        let native = read(output.stdout);
        assert_native(&native, 2);
        assert!(native.column(3).is_null(1));
    }

    #[test]
    fn file_directory_and_parallel_output_agree() {
        let fixture = Fixture::new();
        let mut expected = None;
        for threads in ["1", "2"] {
            let out = fixture.root.join(format!("out-{threads}.parquet"));
            let output = command()
                .arg("run")
                .arg(&fixture.view)
                .arg("--input")
                .arg(&fixture.input)
                .args([
                    "--output",
                    "parquet",
                    "--parquet-temporal",
                    "native",
                    "--threads",
                    threads,
                    "--out",
                ])
                .arg(&out)
                .output()
                .unwrap();
            success(&output);
            assert!(output.stdout.is_empty());
            let batch = read(fs::read(out).unwrap());
            assert_native(&batch, 2);
            if let Some(expected) = &expected {
                assert_eq!(&batch, expected);
            } else {
                expected = Some(batch);
            }
        }
        let views = fixture.root.join("views");
        let out = fixture.root.join("outputs");
        fs::create_dir(&views).unwrap();
        for name in ["first", "second"] {
            fs::copy(&fixture.view, views.join(format!("{name}.json"))).unwrap();
        }
        let output = command()
            .arg("run")
            .arg(&views)
            .arg("--input")
            .arg(&fixture.input)
            .args([
                "--output",
                "parquet",
                "--parquet-temporal",
                "native",
                "--out",
            ])
            .arg(&out)
            .output()
            .unwrap();
        success(&output);
        for name in ["first", "second"] {
            assert_eq!(
                &read(fs::read(out.join(format!("{name}.parquet"))).unwrap()),
                expected.as_ref().unwrap()
            );
        }
    }

    #[test]
    fn invalid_value_warns_on_stderr_without_corrupting_stdout() {
        let fixture = Fixture::new();
        let mut input = resources("Patient");
        input[0]["birthDate"] = json!("SECRET_INVALID_DATE");
        fixture.write_input(input);
        let output = fixture.run("parquet", Some("native"));
        success(&output);
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("Invalid temporal value"));
        assert!(stderr.contains("day"));
        assert!(!stderr.contains("SECRET_INVALID_DATE"));
        let batch = read(output.stdout);
        assert!(batch.column(1).is_null(0));
        assert_eq!(batch.num_rows(), 2);
    }

    #[tokio::test]
    async fn postgres_uses_the_same_native_writer() {
        let Ok(url) = std::env::var("SOF_CONFORMANCE_DB") else {
            eprintln!("SOF_CONFORMANCE_DB not set — skipping PostgreSQL CLI smoke");
            return;
        };
        let fixture = Fixture::new();
        let resource_type = format!("SofTemporalSmoke{}", std::process::id());
        let table = resource_type.to_lowercase();
        let pool = sqlx_postgres::PgPool::connect(&url).await.unwrap();
        sqlx_core::raw_sql::raw_sql(sqlx_core::sql_str::AssertSqlSafe(format!(
            "CREATE TABLE {table} (resource JSONB, status TEXT DEFAULT 'created')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        for resource in resources(&resource_type) {
            sqlx_core::query::query(sqlx_core::sql_str::AssertSqlSafe(format!(
                "INSERT INTO {table} (resource) VALUES ($1)"
            )))
            .bind(resource)
            .execute(&pool)
            .await
            .unwrap();
        }
        fs::write(&fixture.view, view(&resource_type).to_string()).unwrap();
        let output = command()
            .arg("run")
            .arg(&fixture.view)
            .arg("--db")
            .arg(&url)
            .args(["--output", "parquet", "--parquet-temporal", "native"])
            .output()
            .unwrap();
        sqlx_core::raw_sql::raw_sql(sqlx_core::sql_str::AssertSqlSafe(format!(
            "DROP TABLE {table}"
        )))
        .execute(&pool)
        .await
        .unwrap();
        success(&output);
        assert_native(&read(output.stdout), 2);
    }
}
