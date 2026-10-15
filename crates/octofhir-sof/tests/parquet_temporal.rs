//! Round trips through the public writer, not the private conversion helpers.
#![cfg(feature = "parquet")]

use arrow::array::{
    Array, Date32Array, StringArray, Time64MicrosecondArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, TimeUnit};
use arrow::record_batch::{RecordBatch, RecordBatchReader};
use bytes::Bytes;
use octofhir_sof::output::{
    AsyncOutputWriter, AsyncWriter, OutputWriter, ParquetCompression, ParquetWriter,
    TemporalEncoding, get_writer,
};
use octofhir_sof::{ColumnInfo, ColumnType, Error, ViewResult};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde_json::{Value, json};

fn result(ty: ColumnType, values: Vec<Value>) -> ViewResult {
    let row_count = values.len();
    ViewResult {
        columns: vec![ColumnInfo::new("temporal", ty)],
        data: values.into_iter().map(|value| vec![value]).collect(),
        row_count,
    }
}

fn read(bytes: Vec<u8>) -> RecordBatch {
    let reader = ParquetRecordBatchReaderBuilder::try_new(Bytes::from(bytes))
        .unwrap()
        .build()
        .unwrap();
    let schema = reader.schema();
    let mut batches: Vec<_> = reader.map(Result::unwrap).collect();
    assert!(batches.len() <= 1);
    batches
        .pop()
        .unwrap_or_else(|| RecordBatch::new_empty(schema))
}

fn round_trip(writer: &dyn OutputWriter, result: &ViewResult) -> RecordBatch {
    let mut bytes = Vec::new();
    writer.write(result, &mut bytes).unwrap();
    read(bytes)
}

fn native() -> ParquetWriter {
    ParquetWriter::new().with_temporal(TemporalEncoding::Native)
}

#[test]
fn encoding_configuration_and_text_factories() {
    assert_eq!(TemporalEncoding::default(), TemporalEncoding::Text);
    assert_eq!(ParquetWriter::default().temporal, TemporalEncoding::Text);
    assert_eq!(
        TemporalEncoding::parse(" STRING ").unwrap(),
        TemporalEncoding::Text
    );
    assert_eq!(
        " Native ".parse::<TemporalEncoding>().unwrap(),
        TemporalEncoding::Native
    );
    assert!(TemporalEncoding::parse("epoch").is_err());
    let AsyncWriter::Parquet(async_default) = AsyncWriter::new("parquet").unwrap() else {
        panic!("wrong factory writer")
    };
    let sync_default = get_writer("parquet").unwrap();
    for ty in [
        ColumnType::Date,
        ColumnType::DateTime,
        ColumnType::Instant,
        ColumnType::Time,
    ] {
        let r = result(
            ty,
            vec![
                json!("2026"),
                json!("2026-03"),
                json!("invalid"),
                Value::Null,
            ],
        );
        for w in [
            &ParquetWriter::new() as &dyn OutputWriter,
            &ParquetWriter::new().with_temporal(TemporalEncoding::Text),
            &async_default,
            sync_default.as_ref(),
        ] {
            let batch = round_trip(w, &r);
            assert_eq!(batch.schema().field(0).data_type(), &DataType::Utf8);
            let strings = batch
                .column(0)
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            assert_eq!(
                strings.iter().collect::<Vec<_>>(),
                vec![Some("2026"), Some("2026-03"), Some("invalid"), None]
            );
        }
    }
}

#[test]
fn native_dates_floor_precision_and_preserve_negative_epoch() {
    let values = [
        "1970",
        "1970-02",
        "1970-01-01",
        "1970-01-02",
        "1969-12-31",
        "2000-02-29",
        "2026-03",
        "2026",
    ];
    let batch = round_trip(
        &native(),
        &result(ColumnType::Date, values.iter().map(|v| json!(v)).collect()),
    );
    assert_eq!(batch.schema().field(0).data_type(), &DataType::Date32);
    let dates = batch
        .column(0)
        .as_any()
        .downcast_ref::<Date32Array>()
        .unwrap();
    assert_eq!(
        dates.values().as_ref(),
        &[0, 31, 0, 1, -1, 11016, 20513, 20454]
    );
}

#[test]
fn native_timestamps_normalize_offsets_and_truncate_submicroseconds() {
    let values = [
        "1970",
        "1970-01",
        "1970-01-02",
        "1970-01-01T00:00:00Z",
        "1970-01-01T00:00:01.123456789Z",
        "1969-12-31T23:00:00-01:00",
        "1970-01-01T01:00:00+02:00",
        "1970-01-01T00:00:02",
        "1970-01-01T00:01",
        "1969-12-31T23:59:59.999999Z",
    ];
    let expected = [
        0,
        0,
        86_400_000_000,
        0,
        1_123_456,
        0,
        -3_600_000_000,
        2_000_000,
        60_000_000,
        -1,
    ];
    for ty in [ColumnType::DateTime, ColumnType::Instant] {
        let batch = round_trip(
            &native(),
            &result(ty, values.iter().map(|v| json!(v)).collect()),
        );
        assert_eq!(
            batch.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into()))
        );
        let times = batch
            .column(0)
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(times.values().as_ref(), &expected);
    }
}

#[test]
fn native_time_uses_microseconds_since_midnight() {
    let values = [
        "00:00",
        "00:00:00",
        "00:00:01.123456789",
        "13:45:30",
        "23:59:59.999999",
    ];
    let batch = round_trip(
        &native(),
        &result(ColumnType::Time, values.iter().map(|v| json!(v)).collect()),
    );
    assert_eq!(
        batch.schema().field(0).data_type(),
        &DataType::Time64(TimeUnit::Microsecond)
    );
    let times = batch
        .column(0)
        .as_any()
        .downcast_ref::<Time64MicrosecondArray>()
        .unwrap();
    assert_eq!(
        times.values().as_ref(),
        &[0, 0, 1_123_456, 49_530_000_000, 86_399_999_999]
    );
}

#[test]
fn invalid_missing_and_empty_temporals() {
    for ty in [
        ColumnType::Date,
        ColumnType::DateTime,
        ColumnType::Instant,
        ColumnType::Time,
    ] {
        let mut r = result(
            ty,
            vec![
                json!(""),
                json!("noon"),
                json!("2026-02-30"),
                json!("25:00:00"),
                json!(12),
                json!({}),
                json!([]),
                Value::Null,
            ],
        );
        r.data.push(vec![]);
        r.row_count += 1;
        let batch = round_trip(&native(), &r);
        assert_eq!(batch.num_rows(), 9);
        assert_eq!(batch.column(0).null_count(), 9);
        let empty = round_trip(&native(), &result(ty, vec![]));
        assert_eq!(empty.num_rows(), 0);
        assert_eq!(empty.schema(), batch.schema());
        let nulls = round_trip(&native(), &result(ty, vec![Value::Null; 2]));
        assert_eq!(nulls.column(0).null_count(), 2);
        r.columns[0].nullable = false;
        let error = OutputWriter::write(&native(), &r, &mut Vec::new()).unwrap_err();
        assert!(matches!(error, Error::Output(_)));
    }
}

#[test]
fn other_types_and_nullability_are_unchanged() {
    let cases = [
        (ColumnType::String, json!("string")),
        (ColumnType::Integer, json!(12)),
        (ColumnType::Integer64, json!(9_000_000_000_i64)),
        (ColumnType::Decimal, json!(1.5)),
        (ColumnType::Boolean, json!(true)),
        (ColumnType::Base64Binary, json!("YQ==")),
        (ColumnType::Json, json!(["2026", "2026-03"])),
    ];
    for (ty, value) in cases {
        let mut r = result(ty, vec![value]);
        r.columns[0].nullable = false;
        let text = round_trip(&ParquetWriter::new(), &r);
        for codec in [
            ParquetCompression::None,
            ParquetCompression::Snappy,
            ParquetCompression::Gzip,
            ParquetCompression::Zstd,
        ] {
            let batch = round_trip(&native().with_compression(codec), &r);
            assert_eq!(batch, text, "type {ty}");
            assert!(!batch.schema().field(0).is_nullable());
        }
    }
}

#[tokio::test]
async fn async_buffer_and_stream_share_native_encoding() {
    let r = result(ColumnType::Date, vec![json!("1970-01"), Value::Null]);
    let expected = round_trip(&native(), &r);
    let AsyncWriter::Parquet(writer) = AsyncWriter::Parquet(native()) else {
        unreachable!()
    };
    let mut bytes = Vec::new();
    AsyncOutputWriter::write(&writer, &r, &mut bytes)
        .await
        .unwrap();
    assert_eq!(read(bytes), expected);
    let mut bytes = Vec::new();
    let stream = Box::pin(futures_util::stream::iter(r.data.clone()));
    writer
        .write_streaming(&r.columns, stream, &mut bytes)
        .await
        .unwrap();
    assert_eq!(read(bytes), expected);
    let mut bytes = Vec::new();
    writer
        .write_streaming(
            &r.columns,
            Box::pin(futures_util::stream::empty()),
            &mut bytes,
        )
        .await
        .unwrap();
    let empty = read(bytes);
    assert_eq!(empty.num_rows(), 0);
    assert_eq!(empty.schema(), expected.schema());
}
