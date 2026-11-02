//! Row-at-a-time encoding for streamed output (exports, large results).
//!
//! [`RowEncoder`] turns rows into the bytes of one output file without
//! holding the result in memory and without doing any I/O itself: the host
//! writes what [`RowEncoder::start`], [`RowEncoder::row`] and
//! [`RowEncoder::finish`] return to wherever the file goes (a local file, an
//! object store, an HTTP body). A row is a JSON object whose keys are the
//! column names; JSON text in column order (PostgreSQL `row_to_json`, DuckDB
//! `to_json`) is passed through unchanged for `ndjson` and `json`.

use serde_json::{Map, Value};

use crate::{Error, Result};

/// The format of a streamed output file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowFormat {
    /// One JSON object per line.
    Ndjson,
    /// RFC 4180 CSV, with a header row when `header` is set.
    Csv { header: bool },
    /// One JSON array of objects.
    Json,
}

impl RowFormat {
    /// The file extension of the format.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Ndjson => "ndjson",
            Self::Csv { .. } => "csv",
            Self::Json => "json",
        }
    }

    /// The media type of the format.
    pub fn content_type(self) -> &'static str {
        match self {
            Self::Ndjson => "application/x-ndjson",
            Self::Csv { .. } => "text/csv",
            Self::Json => "application/json",
        }
    }
}

/// Encodes the rows of one output file.
#[derive(Debug)]
pub struct RowEncoder {
    format: RowFormat,
    columns: Vec<String>,
    rows: usize,
}

impl RowEncoder {
    /// An encoder for a file of `format` with these result columns.
    pub fn new(format: RowFormat, columns: Vec<String>) -> Self {
        Self {
            format,
            columns,
            rows: 0,
        }
    }

    /// Rows encoded so far.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The result columns.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// What the file starts with: the CSV header, or `[` for JSON.
    pub fn start(&self) -> String {
        match self.format {
            RowFormat::Csv { header: true } => {
                let mut line = self
                    .columns
                    .iter()
                    .map(|column| csv_field(column))
                    .collect::<Vec<_>>()
                    .join(",");
                line.push_str("\r\n");
                line
            }
            RowFormat::Json => "[".into(),
            _ => String::new(),
        }
    }

    /// One row, given as JSON object text in column order.
    pub fn row_json(&mut self, row: &str) -> Result<String> {
        let encoded = match self.format {
            RowFormat::Ndjson => format!("{row}\n"),
            RowFormat::Json if self.rows == 0 => row.to_owned(),
            RowFormat::Json => format!(",{row}"),
            RowFormat::Csv { .. } => {
                let object: Map<String, Value> = serde_json::from_str(row).map_err(|error| {
                    Error::Output(format!("A row is not a JSON object: {error}"))
                })?;
                self.csv_line(&object)
            }
        };
        self.rows += 1;
        Ok(encoded)
    }

    /// One row, given as a JSON object.
    pub fn row(&mut self, row: &Map<String, Value>) -> String {
        let encoded = match self.format {
            RowFormat::Csv { .. } => self.csv_line(row),
            _ => {
                let mut ordered = String::from("{");
                for (index, column) in self.columns.iter().enumerate() {
                    if index > 0 {
                        ordered.push(',');
                    }
                    ordered.push_str(&Value::String(column.clone()).to_string());
                    ordered.push(':');
                    ordered.push_str(&row.get(column).unwrap_or(&Value::Null).to_string());
                }
                ordered.push('}');
                match self.format {
                    RowFormat::Ndjson => format!("{ordered}\n"),
                    _ if self.rows == 0 => ordered,
                    _ => format!(",{ordered}"),
                }
            }
        };
        self.rows += 1;
        encoded
    }

    /// What the file ends with: `]` for JSON.
    pub fn finish(&self) -> String {
        match self.format {
            RowFormat::Json => "]\n".into(),
            _ => String::new(),
        }
    }

    fn csv_line(&self, row: &Map<String, Value>) -> String {
        let mut line = self
            .columns
            .iter()
            .map(|column| csv_value(row.get(column).unwrap_or(&Value::Null)))
            .collect::<Vec<_>>()
            .join(",");
        line.push_str("\r\n");
        line
    }
}

/// A CSV field: quoted when it holds a comma, quote or line break.
fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

/// A JSON value as a CSV field: null is empty, a string as is, anything else
/// as its JSON text.
fn csv_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => csv_field(text),
        other => csv_field(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn columns() -> Vec<String> {
        vec!["id".into(), "note".into(), "n".into()]
    }

    fn encode(format: RowFormat, rows: &[&str]) -> String {
        let mut encoder = RowEncoder::new(format, columns());
        let mut out = encoder.start();
        for row in rows {
            out.push_str(&encoder.row_json(row).unwrap());
        }
        out.push_str(&encoder.finish());
        out
    }

    const ROWS: [&str; 2] = [
        r#"{"id":"a","note":"x, \"y\"","n":1}"#,
        r#"{"id":"b","note":null,"n":{"k":2}}"#,
    ];

    #[test]
    fn rows_stream_in_each_format() {
        assert_eq!(
            encode(RowFormat::Ndjson, &ROWS),
            format!("{}\n{}\n", ROWS[0], ROWS[1])
        );
        assert_eq!(
            encode(RowFormat::Json, &ROWS),
            format!("[{},{}]\n", ROWS[0], ROWS[1])
        );
        assert_eq!(encode(RowFormat::Json, &[]), "[]\n");
        assert_eq!(
            encode(RowFormat::Csv { header: true }, &ROWS),
            "id,note,n\r\na,\"x, \"\"y\"\"\",1\r\nb,,\"{\"\"k\"\":2}\"\r\n"
        );
        assert_eq!(encode(RowFormat::Csv { header: false }, &[]), "");
    }

    #[test]
    fn object_rows_keep_column_order() {
        let mut encoder = RowEncoder::new(RowFormat::Ndjson, columns());
        let row = json!({"n": 3, "id": "c"});
        assert_eq!(
            encoder.row(row.as_object().unwrap()),
            "{\"id\":\"c\",\"note\":null,\"n\":3}\n"
        );
        assert_eq!(encoder.rows(), 1);
    }
}
