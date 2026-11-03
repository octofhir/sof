//! SQLQuery jobs in an embedded DuckDB (feature `duckdb`).
//!
//! [`DuckDbJob`] runs a resolved job ([`crate::plan`]) without a database
//! server: every ViewDefinition dependency is evaluated by the in-memory
//! engine over the given resources and its rows loaded into a table typed by
//! [`Dialect::value_type`] (DuckDB), every ValueSet and ConceptMap relation
//! into a table of its fixed columns ([`crate::relations`]); the query then
//! reads each dependency under its label, as a CTE, with SQLViews as CTEs
//! over their own dependencies. Parameters are bound positionally, cast to
//! their declared types ([`positional_parameters`]), never interpolated.
//!
//! [`DuckDbJob::sandbox`] turns off DuckDB's file and network access and
//! locks the configuration, so the query reads nothing but the job's tables:
//! call it before running SQL from someone else.

use std::collections::HashMap;

use duckdb::arrow::datatypes::DataType;
use duckdb::types::Value as DuckValue;
use duckdb::{Connection, params_from_iter};
use serde_json::{Map, Value};

use crate::library::{ParameterValue, SqlLibrary, positional_parameters};
use crate::plan::{Artifact, Edge, ResolvedJob, TerminologyKind, closure};
use crate::relations::{CONCEPT_MAP_COLUMNS, VALUE_SET_COLUMNS};
use crate::{
    ColumnInfo, ColumnType, CompileOptions, Dialect, Error, Result, execute_parallel_with,
};
use crate::{ViewDefinition, ViewResult};

/// Alias of the query in the statement around it.
const QUERY_ALIAS: &str = "_sof_q";

/// Receives the result of a query: its columns, then each row as a JSON
/// object in column order.
pub trait RowSink {
    fn start(&mut self, columns: &[ColumnInfo]) -> Result<()>;
    fn row(&mut self, row: Map<String, Value>) -> Result<()>;
}

/// A job's tables in one DuckDB database.
pub struct DuckDbJob {
    connection: Connection,
    /// Artifact index → its table.
    tables: HashMap<usize, String>,
}

impl DuckDbJob {
    /// A job in a new in-memory database.
    pub fn open_in_memory() -> Result<Self> {
        Connection::open_in_memory()
            .map(Self::with_connection)
            .map_err(execution)
    }

    /// A job in an existing database (its own tables stay readable to the
    /// query unless [`Self::sandbox`] hides nothing: label CTEs shadow them).
    pub fn with_connection(connection: Connection) -> Self {
        Self {
            connection,
            tables: HashMap::new(),
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Load the tables of `job`: each ViewDefinition evaluated over
    /// `resources` on up to `threads` threads (resources of other types are
    /// skipped), each terminology relation's rows. SQLViews need no table.
    ///
    /// Evaluation blocks: from an async runtime, call it where blocking is
    /// allowed (`spawn_blocking`, `block_in_place`).
    pub fn load(
        &mut self,
        job: &ResolvedJob,
        resources: &[Value],
        threads: usize,
        options: &CompileOptions,
    ) -> Result<()> {
        for (index, artifact) in job.artifacts.iter().enumerate() {
            if self.tables.contains_key(&index) {
                continue;
            }
            let table = format!("_sof_dep_{index}");
            match artifact {
                Artifact::View { resource } => {
                    let view = ViewDefinition::from_json(resource)?;
                    view.validate()?;
                    let result = execute_parallel_with(&view, resources, threads, options)?;
                    let columns: Vec<(String, &'static str)> = result
                        .columns
                        .iter()
                        .map(|column| {
                            (
                                column.name.clone(),
                                Dialect::DuckDb.value_type(column.col_type),
                            )
                        })
                        .collect();
                    self.create(&table, &columns)?;
                    let mut appender = self.connection.appender(&table).map_err(execution)?;
                    for row in result.data {
                        let values: Vec<DuckValue> = row
                            .into_iter()
                            .zip(&result.columns)
                            .map(|(value, column)| to_duck(value, column.col_type))
                            .collect();
                        appender
                            .append_row(duckdb::appender_params_from_iter(values))
                            .map_err(execution)?;
                    }
                    appender.flush().map_err(execution)?;
                }
                Artifact::Terminology { relation, rows } => {
                    let spec: &[(&str, &str)] = match relation {
                        TerminologyKind::ValueSet => &VALUE_SET_COLUMNS,
                        TerminologyKind::ConceptMap => &CONCEPT_MAP_COLUMNS,
                    };
                    let columns: Vec<(String, &'static str)> = spec
                        .iter()
                        .map(|(name, sql_type)| ((*name).to_owned(), duck_type(sql_type)))
                        .collect();
                    self.create(&table, &columns)?;
                    let mut appender = self.connection.appender(&table).map_err(execution)?;
                    for row in rows.as_array().into_iter().flatten() {
                        let values: Vec<DuckValue> = spec
                            .iter()
                            .map(|(name, sql_type)| {
                                let ty = if *sql_type == "boolean" {
                                    ColumnType::Boolean
                                } else {
                                    ColumnType::String
                                };
                                to_duck(row[*name].clone(), ty)
                            })
                            .collect();
                        appender
                            .append_row(duckdb::appender_params_from_iter(values))
                            .map_err(execution)?;
                    }
                    appender.flush().map_err(execution)?;
                }
                Artifact::SqlView { .. } => continue,
            }
            self.tables.insert(index, table);
        }
        Ok(())
    }

    /// Turn off file and network access and lock the configuration for the
    /// rest of the connection's life.
    pub fn sandbox(&self) -> Result<()> {
        self.connection
            .execute_batch("SET enable_external_access = false; SET lock_configuration = true;")
            .map_err(execution)
    }

    /// Run `library` (a SQLQuery of the job, read in the DuckDB dialect)
    /// over its dependency `edges` with bound `parameters`, handing the
    /// columns and rows to `sink`; returns the row count.
    pub fn run(
        &self,
        job: &ResolvedJob,
        edges: &[Edge],
        library: &SqlLibrary,
        parameters: &HashMap<String, ParameterValue>,
        sink: &mut dyn RowSink,
    ) -> Result<usize> {
        let (ctes, query, values) = self.statement(job, edges, library, parameters)?;
        let query = query.as_str();
        let columns = self.result_columns(&ctes, query, &values, &library.display)?;
        sink.start(&columns)?;
        let temporal: Vec<&str> = columns_of(&columns, ColumnType::DateTime);
        let decimal: Vec<&str> = columns_of(&columns, ColumnType::Decimal);
        let statement =
            format!("{ctes} SELECT to_json({QUERY_ALIAS})::VARCHAR FROM ({query}) {QUERY_ALIAS}");
        let mut prepared = self
            .connection
            .prepare(&statement)
            .map_err(|error| query_error(&library.display, error))?;
        let mut rows = prepared
            .query(params_from_iter(values))
            .map_err(|error| query_error(&library.display, error))?;
        let mut count = 0;
        while let Some(row) = rows
            .next()
            .map_err(|error| query_error(&library.display, error))?
        {
            let text: String = row.get(0).map_err(execution)?;
            let mut object: Map<String, Value> = serde_json::from_str(&text)?;
            for name in &temporal {
                if let Some(Value::String(value)) = object.get_mut(*name) {
                    *value = fhir_date_time(value);
                }
            }
            for name in &decimal {
                if let Some(value) = object.get_mut(*name) {
                    shortest_decimal(value);
                }
            }
            sink.row(object)?;
            count += 1;
        }
        Ok(count)
    }

    /// The result columns of `library` as [`Self::run`] would give them,
    /// without reading a row: a check of the query against the job's tables
    /// (load them over no resources to check without data).
    pub fn describe(
        &self,
        job: &ResolvedJob,
        edges: &[Edge],
        library: &SqlLibrary,
        parameters: &HashMap<String, ParameterValue>,
    ) -> Result<Vec<ColumnInfo>> {
        let (ctes, query, values) = self.statement(job, edges, library, parameters)?;
        self.result_columns(&ctes, &query, &values, &library.display)
    }

    /// The `WITH …` prefix, the query with positional parameters and their
    /// values.
    fn statement(
        &self,
        job: &ResolvedJob,
        edges: &[Edge],
        library: &SqlLibrary,
        parameters: &HashMap<String, ParameterValue>,
    ) -> Result<(String, String, Vec<DuckValue>)> {
        let ctes = self.ctes(job, edges)?;
        let positional = positional_parameters(library, Dialect::DuckDb);
        let query = positional
            .sql
            .trim()
            .trim_end_matches(';')
            .trim_end()
            .to_owned();
        let values = positional
            .parameters
            .iter()
            .map(|name| match parameters.get(name) {
                Some(ParameterValue::Text(text) | ParameterValue::Decimal(text)) => {
                    DuckValue::Text(text.clone())
                }
                Some(ParameterValue::Integer(value)) => DuckValue::BigInt(*value),
                Some(ParameterValue::Boolean(flag)) => DuckValue::Boolean(*flag),
                Some(ParameterValue::Null) | None => DuckValue::Null,
            })
            .collect();
        Ok((ctes, query, values))
    }

    /// [`Self::run`], collected.
    pub fn collect(
        &self,
        job: &ResolvedJob,
        edges: &[Edge],
        library: &SqlLibrary,
        parameters: &HashMap<String, ParameterValue>,
    ) -> Result<ViewResult> {
        #[derive(Default)]
        struct Collected {
            columns: Vec<ColumnInfo>,
            data: Vec<Vec<Value>>,
        }
        impl RowSink for Collected {
            fn start(&mut self, columns: &[ColumnInfo]) -> Result<()> {
                self.columns = columns.to_vec();
                Ok(())
            }
            fn row(&mut self, mut row: Map<String, Value>) -> Result<()> {
                self.data.push(
                    self.columns
                        .iter()
                        .map(|column| row.remove(&column.name).unwrap_or(Value::Null))
                        .collect(),
                );
                Ok(())
            }
        }
        let mut collected = Collected::default();
        let row_count = self.run(job, edges, library, parameters, &mut collected)?;
        Ok(ViewResult {
            columns: collected.columns,
            data: collected.data,
            row_count,
        })
    }

    fn create(&self, table: &str, columns: &[(String, &'static str)]) -> Result<()> {
        let list: Vec<String> = columns
            .iter()
            .map(|(name, sql_type)| format!("{} {sql_type}", quote(name)))
            .collect();
        let list = if list.is_empty() {
            // A view without columns still has rows to count.
            "\"_sof_empty\" BOOLEAN".to_owned()
        } else {
            list.join(", ")
        };
        self.connection
            .execute_batch(&format!("CREATE TEMP TABLE {table} ({list})"))
            .map_err(execution)
    }

    /// The `WITH …` prefix giving each dependency its label.
    fn ctes(&self, job: &ResolvedJob, edges: &[Edge]) -> Result<String> {
        let order =
            closure(job, edges).map_err(|error| Error::InvalidLibrary(error.to_string()))?;
        let mut list = Vec::with_capacity(order.len());
        for (label, artifact) in order {
            let body = match &job.artifacts[artifact] {
                Artifact::SqlView { sql, .. } => sql.trim().trim_end_matches(';').to_owned(),
                _ => {
                    let table = self.tables.get(&artifact).ok_or_else(|| {
                        Error::Execution(format!("dependency {label} was not loaded"))
                    })?;
                    format!("SELECT * FROM {table}")
                }
            };
            list.push(format!("{} AS ({body})", quote(&label)));
        }
        Ok(if list.is_empty() {
            String::new()
        } else {
            format!("WITH {}", list.join(", "))
        })
    }

    /// The query's result columns, checked to be distinct.
    fn result_columns(
        &self,
        ctes: &str,
        query: &str,
        values: &[DuckValue],
        display: &str,
    ) -> Result<Vec<ColumnInfo>> {
        let mut prepared = self
            .connection
            .prepare(&format!(
                "{ctes} SELECT * FROM ({query}) {QUERY_ALIAS} LIMIT 0"
            ))
            .map_err(|error| query_error(display, error))?;
        // Executed (for no rows) only so the result schema is known.
        drop(
            prepared
                .query(params_from_iter(values.iter().cloned()))
                .map_err(|error| query_error(display, error))?,
        );
        let names = prepared.column_names();
        let mut columns: Vec<ColumnInfo> = Vec::with_capacity(names.len());
        for (index, name) in names.into_iter().enumerate() {
            if columns.iter().any(|known| known.name == name) {
                return Err(Error::Execution(format!(
                    "{display}: result column {name} appears twice; give each column a distinct name"
                )));
            }
            columns.push(ColumnInfo {
                col_type: column_type(&prepared.column_type(index)),
                name,
                nullable: true,
                description: None,
            });
        }
        Ok(columns)
    }
}

/// The DuckDB spelling of a relation column type.
fn duck_type(sql_type: &str) -> &'static str {
    match sql_type {
        "boolean" => "BOOLEAN",
        _ => "VARCHAR",
    }
}

/// A row value as DuckDB takes it into a column of `ty` (the table declares
/// [`Dialect::value_type`]; DuckDB casts text to DECIMAL).
fn to_duck(value: Value, ty: ColumnType) -> DuckValue {
    match value {
        Value::Null => DuckValue::Null,
        Value::Bool(flag) => DuckValue::Boolean(flag),
        Value::Number(number) => match (ty, number.as_i64()) {
            (ColumnType::Integer | ColumnType::Integer64, Some(integer)) => {
                DuckValue::BigInt(integer)
            }
            _ => DuckValue::Text(number.to_string()),
        },
        Value::String(text) => DuckValue::Text(text),
        other @ (Value::Array(_) | Value::Object(_)) => DuckValue::Text(other.to_string()),
    }
}

/// The column type of a result column.
fn column_type(data_type: &DataType) -> ColumnType {
    match data_type {
        DataType::Boolean => ColumnType::Boolean,
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::UInt8 | DataType::UInt16 => {
            ColumnType::Integer
        }
        DataType::Int64 | DataType::UInt32 | DataType::UInt64 => ColumnType::Integer64,
        DataType::Float16
        | DataType::Float32
        | DataType::Float64
        | DataType::Decimal32(..)
        | DataType::Decimal64(..)
        | DataType::Decimal128(..)
        | DataType::Decimal256(..) => ColumnType::Decimal,
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => ColumnType::String,
        DataType::Date32 | DataType::Date64 => ColumnType::Date,
        DataType::Timestamp(..) => ColumnType::DateTime,
        DataType::Time32(..) | DataType::Time64(..) => ColumnType::Time,
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => ColumnType::Base64Binary,
        _ => ColumnType::Json,
    }
}

fn columns_of(columns: &[ColumnInfo], ty: ColumnType) -> Vec<&str> {
    columns
        .iter()
        .filter(|column| column.col_type == ty)
        .map(|column| column.name.as_str())
        .collect()
}

/// A DECIMAL(38,9) read back as `142.000000000` is the number 142, written
/// without the scale's zeros.
fn shortest_decimal(value: &mut Value) {
    if let Some(number) = value.as_f64()
        && number.fract() == 0.0
        && number.abs() < 9_007_199_254_740_992.0
    {
        *value = Value::from(number as i64);
    }
}

/// DuckDB's `2024-01-02 03:04:05[.f][+hh[:mm]]` as a FHIR dateTime.
fn fhir_date_time(value: &str) -> String {
    let mut text = value.replacen(' ', "T", 1);
    let offset_at = text
        .rfind(['+', '-'])
        .filter(|&at| at > text.find('T').unwrap_or(text.len()));
    if let Some(at) = offset_at
        && text.len() - at == 3
    {
        text.push_str(":00");
    }
    text
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn execution(error: duckdb::Error) -> Error {
    Error::Execution(error.to_string())
}

fn query_error(display: &str, error: duckdb::Error) -> Error {
    Error::Execution(format!("{display}: {error}"))
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use serde_json::json;

    use super::*;
    use crate::expansion::Resolved;
    use crate::library::bind_parameters;
    use crate::plan::{ArtifactSource, ContextArtifacts, PlanError, Resolver, TerminologyContent};

    struct Nothing;

    #[async_trait::async_trait]
    impl ArtifactSource for Nothing {
        async fn artifact(&self, canonical: &str) -> std::result::Result<Resolved, PlanError> {
            Err(PlanError::NotFound(canonical.to_owned()))
        }

        async fn value_set(
            &self,
            _label: &str,
            value_set: &Resolved,
            _context: &ContextArtifacts,
        ) -> std::result::Result<TerminologyContent, PlanError> {
            let rows = crate::relations::value_set_rows(&value_set.resource["expansion"])
                .map_err(PlanError::Unprocessable)?;
            Ok(TerminologyContent {
                rows,
                provenance: json!({}),
            })
        }
    }

    fn library(sql: &str, dependencies: Value, parameters: Value) -> Value {
        json!({
            "resourceType": "Library", "url": "http://x/Library/q",
            "type": {"coding": [{"system": crate::library::LIBRARY_TYPES, "code": "sql-query"}]},
            "relatedArtifact": dependencies,
            "parameter": parameters,
            "content": [{"contentType": "application/sql", "data": STANDARD.encode(sql)}]
        })
    }

    #[tokio::test]
    async fn a_query_reads_views_value_sets_and_parameters() {
        let context = ContextArtifacts::from_resources(vec![
            json!({"resourceType": "ViewDefinition", "url": "http://x/ViewDefinition/conditions",
                   "status": "active", "resource": "Condition", "select": [{"column": [
                       {"name": "id", "path": "getResourceKey()"},
                       {"name": "system", "path": "code.coding.first().system"},
                       {"name": "code", "path": "code.coding.first().code"},
                       {"name": "onset", "path": "onset.ofType(dateTime)", "type": "dateTime"},
                       {"name": "score", "path": "extension('http://x/score').value.ofType(decimal)", "type": "decimal"}]}]}),
            json!({"resourceType": "ValueSet", "url": "http://x/vs", "version": "1",
                   "expansion": {"contains": [{"system": "http://snomed.info/sct", "code": "1"}]}}),
            json!({"resourceType": "ConceptMap", "url": "http://x/cm", "group": [{"source": "http://snomed.info/sct",
                   "target": "http://x/local", "element": [{"code": "1", "target": [{"code": "L1", "relationship": "equivalent"}]}]}]}),
        ])
        .unwrap();
        let query = SqlLibrary::from_json(
            &library(
                "SELECT c.id, c.onset, c.score, m.target_code, :min AS min \
                 FROM conditions c JOIN cvd ON cvd.system = c.system AND cvd.code = c.code \
                 JOIN map m ON m.source_code = c.code \
                 WHERE c.onset >= :since AND c.score >= :min ORDER BY c.id;",
                json!([
                    {"type": "depends-on", "label": "conditions", "resource": "http://x/ViewDefinition/conditions"},
                    {"type": "depends-on", "label": "cvd", "resource": "http://x/vs|1"},
                    {"type": "depends-on", "label": "map", "resource": "http://x/cm"}
                ]),
                json!([
                    {"name": "since", "type": "date", "use": "in", "min": 1, "max": "1"},
                    {"name": "min", "type": "decimal", "use": "in", "min": 0, "max": "1"}
                ]),
            ),
            "duckdb",
        )
        .unwrap();
        let mut resolver = Resolver::new(&Nothing, &context, "duckdb");
        let edges = resolver.subject(&query).await.unwrap();
        let job = resolver.finish().unwrap();

        let condition = |id: &str, code: &str, onset: &str, score: f64| {
            json!({"resourceType": "Condition", "id": id,
                   "code": {"coding": [{"system": "http://snomed.info/sct", "code": code}]},
                   "onsetDateTime": onset,
                   "extension": [{"url": "http://x/score", "valueDecimal": score}]})
        };
        let resources = vec![
            condition("a", "1", "2026-02-01T10:00:00Z", 2.5),
            condition("b", "1", "2025-01-01T00:00:00Z", 9.0),
            condition("c", "2", "2026-03-01T00:00:00Z", 9.0),
            condition("d", "1", "2026-04-01T00:00:00Z", 0.5),
            json!({"resourceType": "Patient", "id": "p"}),
        ];
        let mut duck = DuckDbJob::open_in_memory().unwrap();
        duck.load(&job, &resources, 2, &CompileOptions::default())
            .unwrap();
        duck.sandbox().unwrap();
        let parameters = bind_parameters(
            &query,
            Some(&json!({"parameter": [
                {"name": "since", "valueDate": "2026-01-01"},
                {"name": "min", "valueDecimal": 1.5}
            ]})),
        )
        .unwrap();
        let result = duck.collect(&job, &edges, &query, &parameters).unwrap();
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "onset", "score", "target_code", "min"]);
        assert_eq!(result.columns[2].col_type, ColumnType::Decimal);
        assert_eq!(
            result.data,
            vec![vec![
                json!("a"),
                json!("2026-02-01T10:00:00Z"),
                json!(2.5),
                json!("L1"),
                json!(1.5)
            ]]
        );
        // External access is off.
        assert!(
            duck.connection()
                .execute_batch("COPY (SELECT 1) TO '/tmp/sof-should-not-exist.csv'")
                .is_err()
        );
    }

    #[test]
    fn duckdb_timestamps_read_as_fhir_date_times() {
        assert_eq!(fhir_date_time("2026-02-01 10:00:00"), "2026-02-01T10:00:00");
        assert_eq!(
            fhir_date_time("2026-02-01 10:00:00.5+02"),
            "2026-02-01T10:00:00.5+02:00"
        );
        assert_eq!(fhir_date_time("2026-02-01"), "2026-02-01");
    }
}
