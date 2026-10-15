//! Parquet output writer for view results.
//!
//! This module is only available when the `parquet` feature is enabled.
//! Temporal values remain UTF-8 strings by default. Opt in to
//! [`TemporalEncoding::Native`] for dates, UTC timestamps and times in native
//! Arrow types, flooring reduced-precision values to the start of their period.

use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use arrow::array::{
    ArrayRef, BooleanArray, Date32Array, Float64Array, Int64Array, StringArray,
    Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::Compression;
use parquet::file::properties::WriterProperties;

use super::{AsyncOutputWriter, OutputWriter};
use crate::column::{ColumnInfo, ColumnType};
use crate::runner::ViewResult;
use crate::{Error, Result};

/// Parquet compression codecs.
#[derive(Debug, Clone, Copy, Default)]
pub enum ParquetCompression {
    /// No compression.
    None,
    /// Snappy compression (default, good balance of speed and ratio).
    #[default]
    Snappy,
    /// Gzip compression (better ratio, slower).
    Gzip,
    /// LZ4 compression (faster, lower ratio).
    Lz4,
    /// Zstd compression (good balance).
    Zstd,
}

impl From<ParquetCompression> for Compression {
    fn from(compression: ParquetCompression) -> Self {
        match compression {
            ParquetCompression::None => Compression::UNCOMPRESSED,
            ParquetCompression::Snappy => Compression::SNAPPY,
            ParquetCompression::Gzip => Compression::GZIP(Default::default()),
            ParquetCompression::Lz4 => Compression::LZ4,
            ParquetCompression::Zstd => Compression::ZSTD(Default::default()),
        }
    }
}

/// How FHIR temporal columns are encoded in Parquet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TemporalEncoding {
    /// Preserve the original FHIR strings, including their precision and offset.
    #[default]
    Text,
    /// Write `date` as Date32, `dateTime`/`instant` as UTC microsecond timestamps
    /// and `time` as Time64 microseconds. Partial dates floor to period start;
    /// timestamps without an offset are treated as UTC. Invalid values become
    /// null with a warning. Precision finer than microseconds is truncated.
    Native,
}

impl TemporalEncoding {
    /// Parse `text` (or `string`) and `native`, ignoring case and surrounding
    /// whitespace. Other values return an output error.
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "text" | "string" => Ok(Self::Text),
            "native" => Ok(Self::Native),
            _ => Err(Error::Output(format!(
                "Unknown temporal encoding: {value} (expected text or native)"
            ))),
        }
    }
}

impl std::str::FromStr for TemporalEncoding {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}

/// Parquet output writer configuration.
#[derive(Debug, Clone, Default)]
pub struct ParquetWriter {
    /// Compression codec to use.
    pub compression: ParquetCompression,
    /// Encoding of date, dateTime, instant and time columns (text by default).
    pub temporal: TemporalEncoding,
}

impl ParquetWriter {
    /// Create a new Parquet writer with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the compression codec.
    pub fn with_compression(mut self, compression: ParquetCompression) -> Self {
        self.compression = compression;
        self
    }

    /// Set how temporal columns are encoded.
    pub fn with_temporal(mut self, temporal: TemporalEncoding) -> Self {
        self.temporal = temporal;
        self
    }

    /// Convert a column type to an Arrow data type.
    fn column_to_arrow_type(col_type: ColumnType, temporal: TemporalEncoding) -> DataType {
        if temporal == TemporalEncoding::Native {
            match col_type {
                ColumnType::Date => return DataType::Date32,
                ColumnType::DateTime | ColumnType::Instant => {
                    return DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()));
                }
                ColumnType::Time => return DataType::Time64(TimeUnit::Microsecond),
                _ => {}
            }
        }
        match col_type {
            ColumnType::Integer | ColumnType::Integer64 => DataType::Int64,
            ColumnType::Decimal => DataType::Float64,
            ColumnType::Boolean => DataType::Boolean,
            // All other types are stored as strings
            ColumnType::String
            | ColumnType::Date
            | ColumnType::DateTime
            | ColumnType::Instant
            | ColumnType::Time
            | ColumnType::Base64Binary
            | ColumnType::Json => DataType::Utf8,
        }
    }

    /// Build an Arrow schema from column info.
    fn build_schema(columns: &[ColumnInfo], temporal: TemporalEncoding) -> Schema {
        let fields: Vec<Field> = columns
            .iter()
            .map(|c| {
                Field::new(
                    &c.name,
                    Self::column_to_arrow_type(c.col_type, temporal),
                    c.nullable,
                )
            })
            .collect();

        Schema::new(fields)
    }

    /// Build Arrow arrays from result data.
    fn build_arrays(
        columns: &[ColumnInfo],
        data: &[Vec<Value>],
        temporal: TemporalEncoding,
    ) -> Result<Vec<ArrayRef>> {
        columns
            .iter()
            .enumerate()
            .map(|(i, col)| {
                let values: Vec<Option<&Value>> = data
                    .iter()
                    .map(|row| row.get(i).filter(|v| !v.is_null()))
                    .collect();

                Self::values_to_array(col, &values, temporal)
            })
            .collect()
    }

    /// Convert JSON values to an Arrow array.
    fn values_to_array(
        col: &ColumnInfo,
        values: &[Option<&Value>],
        temporal: TemporalEncoding,
    ) -> Result<ArrayRef> {
        if temporal == TemporalEncoding::Native {
            match col.col_type {
                ColumnType::Date => {
                    let arr: Date32Array = values
                        .iter()
                        .map(|value| Self::temporal_value(col, *value, temporal::date_to_days))
                        .collect();
                    return Ok(Arc::new(arr));
                }
                ColumnType::DateTime | ColumnType::Instant => {
                    let arr = values
                        .iter()
                        .map(|value| {
                            Self::temporal_value(col, *value, temporal::datetime_to_micros_utc)
                        })
                        .collect::<TimestampMicrosecondArray>()
                        .with_timezone("UTC");
                    return Ok(Arc::new(arr));
                }
                ColumnType::Time => {
                    let arr: Time64MicrosecondArray = values
                        .iter()
                        .map(|value| Self::temporal_value(col, *value, temporal::time_to_micros))
                        .collect();
                    return Ok(Arc::new(arr));
                }
                _ => {}
            }
        }
        match col.col_type {
            ColumnType::Integer | ColumnType::Integer64 => {
                let arr: Int64Array = values.iter().map(|v| v.and_then(|v| v.as_i64())).collect();
                Ok(Arc::new(arr))
            }
            ColumnType::Decimal => {
                let arr: Float64Array = values.iter().map(|v| v.and_then(|v| v.as_f64())).collect();
                Ok(Arc::new(arr))
            }
            ColumnType::Boolean => {
                let arr: BooleanArray =
                    values.iter().map(|v| v.and_then(|v| v.as_bool())).collect();
                Ok(Arc::new(arr))
            }
            // All string-like types
            _ => {
                let arr: StringArray = values
                    .iter()
                    .map(|v| {
                        v.map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    })
                    .collect();
                Ok(Arc::new(arr))
            }
        }
    }

    /// Null missing values silently; warn about invalid values without exposing
    /// potentially sensitive resource data in logs.
    fn temporal_value<T>(
        col: &ColumnInfo,
        value: Option<&Value>,
        parse: fn(&str) -> Option<T>,
    ) -> Option<T> {
        let value = value?;
        let parsed = value.as_str().and_then(parse);
        if parsed.is_none() {
            tracing::warn!(
                column = %col.name,
                column_type = %col.col_type,
                "Invalid temporal value written as null"
            );
        }
        parsed
    }

    /// Write result data to a buffer as Parquet.
    fn write_to_buffer(&self, result: &ViewResult) -> Result<Vec<u8>> {
        let schema = Arc::new(Self::build_schema(&result.columns, self.temporal));
        let arrays = Self::build_arrays(&result.columns, &result.data, self.temporal)?;

        let record_batch = RecordBatch::try_new(schema.clone(), arrays)
            .map_err(|e| Error::Output(format!("Failed to create record batch: {}", e)))?;

        let mut buffer = Vec::new();
        {
            let props = WriterProperties::builder()
                .set_compression(self.compression.into())
                .build();

            let mut arrow_writer = ArrowWriter::try_new(&mut buffer, schema, Some(props))
                .map_err(|e| Error::Output(format!("Failed to create Parquet writer: {}", e)))?;

            arrow_writer
                .write(&record_batch)
                .map_err(|e| Error::Output(format!("Failed to write record batch: {}", e)))?;

            arrow_writer
                .close()
                .map_err(|e| Error::Output(format!("Failed to close Parquet writer: {}", e)))?;
        }

        Ok(buffer)
    }
}

/// Lenient FHIR temporal parsing for native Parquet output. This is a conversion
/// policy, not FHIR validation; use text output to retain the original precision.
mod temporal {
    use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

    pub fn parse_date(value: &str) -> Option<NaiveDate> {
        let value = value.trim();
        match value.len() {
            4 => NaiveDate::from_ymd_opt(value.parse().ok()?, 1, 1),
            7 => {
                let (year, month) = value.split_once('-')?;
                NaiveDate::from_ymd_opt(year.parse().ok()?, month.parse().ok()?, 1)
            }
            10 => NaiveDate::parse_from_str(value, "%Y-%m-%d").ok(),
            _ => None,
        }
    }

    pub fn date_to_days(value: &str) -> Option<i32> {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1)?;
        i32::try_from(parse_date(value)?.signed_duration_since(epoch).num_days()).ok()
    }

    pub fn datetime_to_micros_utc(value: &str) -> Option<i64> {
        let value = value.trim();
        if value.len() <= 10 {
            return Some(
                parse_date(value)?
                    .and_hms_opt(0, 0, 0)?
                    .and_utc()
                    .timestamp_micros(),
            );
        }
        if let Ok(time) = DateTime::parse_from_rfc3339(value) {
            return Some(time.timestamp_micros());
        }
        for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
            if let Ok(time) = NaiveDateTime::parse_from_str(value, format) {
                return Some(time.and_utc().timestamp_micros());
            }
        }
        None
    }

    pub fn time_to_micros(value: &str) -> Option<i64> {
        let value = value.trim();
        let time = NaiveTime::parse_from_str(value, "%H:%M:%S%.f")
            .or_else(|_| NaiveTime::parse_from_str(value, "%H:%M"))
            .ok()?;
        let micros = i64::from(time.num_seconds_from_midnight()) * 1_000_000
            + i64::from(time.nanosecond() / 1_000);
        // Arrow time-of-day values cannot represent the end-of-day leap second.
        (micros < 86_400_000_000).then_some(micros)
    }
}

impl OutputWriter for ParquetWriter {
    fn content_type(&self) -> &'static str {
        "application/vnd.apache.parquet"
    }

    fn file_extension(&self) -> &'static str {
        "parquet"
    }

    fn write(&self, result: &ViewResult, output: &mut dyn Write) -> Result<()> {
        let buffer = self.write_to_buffer(result)?;
        output
            .write_all(&buffer)
            .map_err(|e| Error::Output(e.to_string()))?;
        output.flush().map_err(|e| Error::Output(e.to_string()))?;
        Ok(())
    }
}

#[async_trait]
impl AsyncOutputWriter for ParquetWriter {
    fn content_type(&self) -> &'static str {
        "application/vnd.apache.parquet"
    }

    fn file_extension(&self) -> &'static str {
        "parquet"
    }

    async fn write<W: AsyncWrite + Unpin + Send>(
        &self,
        result: &ViewResult,
        mut writer: W,
    ) -> Result<()> {
        // Write to buffer first (Parquet writer needs sync write)
        let buffer = self.write_to_buffer(result)?;

        // Write buffer to async writer
        writer
            .write_all(&buffer)
            .await
            .map_err(|e| Error::Output(e.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|e| Error::Output(e.to_string()))?;

        Ok(())
    }

    async fn write_streaming<W: AsyncWrite + Unpin + Send>(
        &self,
        columns: &[ColumnInfo],
        rows: Pin<Box<dyn Stream<Item = Vec<Value>> + Send>>,
        writer: W,
    ) -> Result<()> {
        // Parquet is a columnar format, so we need to buffer all rows
        // to build efficient column chunks
        let mut data = Vec::new();
        let mut rows = rows;
        while let Some(row) = rows.next().await {
            data.push(row);
        }

        let result = ViewResult {
            columns: columns.to_vec(),
            data,
            row_count: 0,
        };

        AsyncOutputWriter::write(self, &result, writer).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parquet_writer_default() {
        let writer = ParquetWriter::new();
        assert!(matches!(writer.compression, ParquetCompression::Snappy));
    }

    #[test]
    fn test_parquet_compression_conversion() {
        assert!(matches!(
            Compression::from(ParquetCompression::None),
            Compression::UNCOMPRESSED
        ));
        assert!(matches!(
            Compression::from(ParquetCompression::Snappy),
            Compression::SNAPPY
        ));
    }

    #[test]
    fn test_column_to_arrow_type() {
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Integer, TemporalEncoding::Text),
            DataType::Int64
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Decimal, TemporalEncoding::Text),
            DataType::Float64
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::Boolean, TemporalEncoding::Text),
            DataType::Boolean
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::String, TemporalEncoding::Text),
            DataType::Utf8
        );
        assert_eq!(
            ParquetWriter::column_to_arrow_type(ColumnType::DateTime, TemporalEncoding::Text),
            DataType::Utf8
        );
    }

    #[test]
    fn test_build_schema() {
        let columns = vec![
            ColumnInfo::new("id", ColumnType::String),
            ColumnInfo::new("value", ColumnType::Integer),
            ColumnInfo::new("active", ColumnType::Boolean),
        ];

        let schema = ParquetWriter::build_schema(&columns, TemporalEncoding::Text);
        assert_eq!(schema.fields().len(), 3);
        assert_eq!(schema.field(0).name(), "id");
        assert_eq!(schema.field(0).data_type(), &DataType::Utf8);
        assert_eq!(schema.field(1).name(), "value");
        assert_eq!(schema.field(1).data_type(), &DataType::Int64);
        assert_eq!(schema.field(2).name(), "active");
        assert_eq!(schema.field(2).data_type(), &DataType::Boolean);
    }

    #[test]
    fn test_parquet_write() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("value", ColumnType::Integer),
            ],
            data: vec![vec![json!("1"), json!(100)], vec![json!("2"), json!(200)]],
            row_count: 2,
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        // Parquet files start with "PAR1" magic bytes
        assert!(output.len() > 4);
        assert_eq!(&output[0..4], b"PAR1");
    }

    #[test]
    fn test_parquet_empty_result() {
        let result = ViewResult {
            columns: vec![ColumnInfo::new("id", ColumnType::String)],
            data: vec![],
            row_count: 0,
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        // Even empty files have the Parquet header
        assert!(output.len() > 4);
        assert_eq!(&output[0..4], b"PAR1");
    }

    #[test]
    fn test_parquet_null_values() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("name", ColumnType::String),
            ],
            data: vec![vec![json!("1"), Value::Null]],
            row_count: 1,
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        OutputWriter::write(&writer, &result, &mut output).unwrap();

        // Should write successfully with null value
        assert!(output.len() > 4);
    }

    #[test]
    fn test_content_type_and_extension() {
        let writer = ParquetWriter::new();
        assert_eq!(
            <ParquetWriter as OutputWriter>::content_type(&writer),
            "application/vnd.apache.parquet"
        );
        assert_eq!(
            <ParquetWriter as OutputWriter>::file_extension(&writer),
            "parquet"
        );
    }

    #[tokio::test]
    async fn test_async_parquet_writer() {
        let result = ViewResult {
            columns: vec![
                ColumnInfo::new("id", ColumnType::String),
                ColumnInfo::new("count", ColumnType::Integer),
            ],
            data: vec![vec![json!("a"), json!(1)], vec![json!("b"), json!(2)]],
            row_count: 2,
        };

        let writer = ParquetWriter::new();
        let mut output = Vec::new();
        AsyncOutputWriter::write(&writer, &result, &mut output)
            .await
            .unwrap();

        assert!(output.len() > 4);
        assert_eq!(&output[0..4], b"PAR1");
    }
}
