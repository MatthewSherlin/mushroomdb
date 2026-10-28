//! Convert [`core_query::ResultSet`] to Arrow RecordBatches and IPC stream bytes.

use arrow_array::{
    ArrayRef, BooleanArray, Date32Array, DurationMicrosecondArray, Float64Array, Int64Array,
    LargeBinaryArray, RecordBatch, RecordBatchOptions, StringArray, TimestampMicrosecondArray,
};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema, TimeUnit};
use core_query::{ResultSet, Value};
use std::sync::Arc;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ColKind {
    Empty,
    Int,
    Float,
    Bool,
    Str,
    Utf8,
    Date,
    LocalDateTime,
    DateTime,
    Duration,
    Binary,
}

fn observe(kind: ColKind, v: &Value) -> ColKind {
    match (kind, v) {
        (ColKind::Empty | ColKind::Int, Value::Int(_)) => ColKind::Int,
        (ColKind::Empty | ColKind::Float, Value::Float(_)) => ColKind::Float,
        (ColKind::Int, Value::Float(_)) | (ColKind::Float, Value::Int(_)) => ColKind::Float,
        (ColKind::Empty | ColKind::Bool, Value::Bool(_)) => ColKind::Bool,
        (ColKind::Empty | ColKind::Str, Value::Str(_)) => ColKind::Str,
        (ColKind::Empty | ColKind::Date, Value::Date(_)) => ColKind::Date,
        (ColKind::Empty | ColKind::LocalDateTime, Value::LocalDateTime(_)) => {
            ColKind::LocalDateTime
        }
        (ColKind::Empty | ColKind::DateTime, Value::DateTime { .. }) => ColKind::DateTime,
        (ColKind::Empty | ColKind::Duration, Value::Duration { .. }) => ColKind::Duration,
        (ColKind::Empty | ColKind::Binary, Value::Binary(_)) => ColKind::Binary,
        (ColKind::Utf8, _) => ColKind::Utf8,
        (_, Value::List(_)) => ColKind::Utf8,
        _ => ColKind::Utf8,
    }
}

/// The value in row `i`, column `col`; a `Value::Null` cell (query-only; a
/// result normally carries a null cell as `None`) is an Arrow null too.
fn cell_at(rs: &ResultSet, i: usize, col: usize) -> Option<&Value> {
    rs.row(i)[col]
        .as_ref()
        .filter(|v| !matches!(v, Value::Null))
}

fn infer_column(rs: &ResultSet, col: usize) -> ColKind {
    let mut kind = ColKind::Empty;
    for i in 0..rs.len() {
        if let Some(v) = cell_at(rs, i, col) {
            kind = observe(kind, v);
        }
    }
    // A month has no fixed length, so Duration(µs) cannot hold it: one
    // duration with months makes the whole column canonical ISO 8601 text.
    if kind == ColKind::Duration && column_has_months(rs, col) {
        return ColKind::Utf8;
    }
    kind
}

fn column_has_months(rs: &ResultSet, col: usize) -> bool {
    (0..rs.len())
        .any(|i| matches!(&rs.row(i)[col], Some(Value::Duration { months, .. }) if *months != 0))
}

fn data_type(kind: ColKind) -> DataType {
    match kind {
        ColKind::Int => DataType::Int64,
        ColKind::Float => DataType::Float64,
        ColKind::Bool => DataType::Boolean,
        ColKind::Date => DataType::Date32,
        ColKind::LocalDateTime => DataType::Timestamp(TimeUnit::Microsecond, None),
        // Arrow carries one time zone per column, so a DateTime column is
        // always UTC: values are UTC instants and each value's own offset is
        // dropped. LOSSY: per-value offsets are not representable.
        ColKind::DateTime => {
            DataType::Timestamp(TimeUnit::Microsecond, Some(DATETIME_COLUMN_TZ.into()))
        }
        ColKind::Duration => DataType::Duration(TimeUnit::Microsecond),
        ColKind::Binary => DataType::LargeBinary,
        ColKind::Empty | ColKind::Str | ColKind::Utf8 => DataType::Utf8,
    }
}

const DATETIME_COLUMN_TZ: &str = "+00:00";

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// Microseconds of a duration as Arrow `Duration(Microsecond)` holds it.
/// LOSSY: nanoseconds below one microsecond are truncated. Months never reach
/// here: a column with any month falls back to Utf8 text (see `infer_column`).
/// Saturates at the `i64` bounds rather than wrapping.
fn duration_micros(days: i32, nanos: i64) -> i64 {
    i64::from(days)
        .saturating_mul(MICROS_PER_DAY)
        .saturating_add(nanos / 1_000)
}

fn canonical_display(v: &Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => {
            let s = format!("{f}");
            if s.contains('.') || s.contains('e') || s.contains('E') {
                s
            } else {
                format!("{s}.0")
            }
        }
        Value::Str(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::List(xs) => {
            let inner: Vec<String> = xs.iter().map(canonical_display).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Map(m) => {
            let inner: Vec<String> = m
                .iter()
                .map(|(k, v)| format!("{k}: {}", canonical_display(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        // Until typed Arrow columns exist, a format-V11 value is its ISO 8601
        // (or base64) text in a Utf8 column.
        Value::Date(_)
        | Value::LocalDateTime(_)
        | Value::DateTime { .. }
        | Value::Duration { .. }
        | Value::Binary(_) => core_query::v11_display_text(v).unwrap_or_default(),
        // A null list element or map value (a top-level null is a null cell).
        Value::Null => "null".to_string(),
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        other => unreachable!("numeric column saw {other:?}"),
    }
}

fn build_column(rs: &ResultSet, col: usize, kind: ColKind) -> ArrayRef {
    match kind {
        ColKind::Int => {
            let vals: Vec<Option<i64>> = (0..rs.len())
                .map(|i| {
                    cell_at(rs, i, col).map(|v| match v {
                        Value::Int(n) => *n,
                        other => unreachable!("int column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(Int64Array::from(vals))
        }
        ColKind::Float => {
            let vals: Vec<Option<f64>> = (0..rs.len())
                .map(|i| cell_at(rs, i, col).map(as_f64))
                .collect();
            Arc::new(Float64Array::from(vals))
        }
        ColKind::Bool => {
            let vals: Vec<Option<bool>> = (0..rs.len())
                .map(|i| {
                    cell_at(rs, i, col).map(|v| match v {
                        Value::Bool(b) => *b,
                        other => unreachable!("bool column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(BooleanArray::from(vals))
        }
        ColKind::Date => {
            let vals: Vec<Option<i32>> = (0..rs.len())
                .map(|i| {
                    cell_at(rs, i, col).map(|v| match v {
                        Value::Date(d) => *d,
                        other => unreachable!("date column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(Date32Array::from(vals))
        }
        ColKind::LocalDateTime => {
            let vals: Vec<Option<i64>> = (0..rs.len())
                .map(|i| {
                    cell_at(rs, i, col).map(|v| match v {
                        Value::LocalDateTime(us) => *us,
                        other => unreachable!("local datetime column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(TimestampMicrosecondArray::from(vals))
        }
        ColKind::DateTime => {
            let vals: Vec<Option<i64>> = (0..rs.len())
                .map(|i| {
                    cell_at(rs, i, col).map(|v| match v {
                        Value::DateTime { micros_utc, .. } => *micros_utc,
                        other => unreachable!("datetime column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(TimestampMicrosecondArray::from(vals).with_timezone(DATETIME_COLUMN_TZ))
        }
        ColKind::Duration => {
            let vals: Vec<Option<i64>> = (0..rs.len())
                .map(|i| {
                    cell_at(rs, i, col).map(|v| match v {
                        Value::Duration { days, nanos, .. } => duration_micros(*days, *nanos),
                        other => unreachable!("duration column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(DurationMicrosecondArray::from(vals))
        }
        ColKind::Binary => {
            let vals: Vec<Option<&[u8]>> = (0..rs.len())
                .map(|i| {
                    cell_at(rs, i, col).map(|v| match v {
                        Value::Binary(b) => b.as_slice(),
                        other => unreachable!("binary column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(LargeBinaryArray::from(vals))
        }
        ColKind::Empty | ColKind::Str | ColKind::Utf8 => {
            let vals: Vec<Option<String>> = (0..rs.len())
                .map(|i| cell_at(rs, i, col).map(canonical_display))
                .collect();
            Arc::new(StringArray::from(vals))
        }
    }
}

/// Infer a per-column Arrow type and build a single-batch [`RecordBatch`].
///
/// Policy: all-Int → Int64; any Float among only numerics → Float64;
/// Bool → Boolean; Str → Utf8; List or mixed types → Utf8 via canonical
/// display; all-null → Utf8 nulls. Null cells stay null.
///
/// Format-V11 types: all-Date → Date32; all-LocalDateTime →
/// Timestamp(µs, no zone); all-DateTime → Timestamp(µs, "+00:00") holding UTC
/// instants (per-value offsets dropped, lossy); all-Duration →
/// Duration(µs) (sub-µs nanos truncated, lossy; a column with any month is Utf8 ISO 8601 text);
/// all-Binary → LargeBinary. A column mixing these with other types falls
/// back to Utf8 text like any other mixed column.
pub fn to_record_batch(rs: &ResultSet) -> Result<RecordBatch, String> {
    let kinds: Vec<ColKind> = (0..rs.columns().len())
        .map(|c| infer_column(rs, c))
        .collect();
    let fields: Vec<Field> = rs
        .columns()
        .iter()
        .zip(&kinds)
        .map(|(name, kind)| Field::new(name, data_type(*kind), true))
        .collect();
    let schema = Arc::new(Schema::new(fields));
    let columns: Vec<ArrayRef> = kinds
        .into_iter()
        .enumerate()
        .map(|(c, kind)| build_column(rs, c, kind))
        .collect();
    // The row count is given, not inferred from the columns, so a result with
    // none (a schema statement's) is a batch too.
    let options = RecordBatchOptions::new().with_row_count(Some(rs.len()));
    RecordBatch::try_new_with_options(schema, columns, &options).map_err(|e| e.to_string())
}

/// Encode `rs` as a single-batch Arrow IPC stream.
pub fn to_ipc_bytes(rs: &ResultSet) -> Result<Vec<u8>, String> {
    let batch = to_record_batch(rs)?;
    let mut buf = Vec::new();
    {
        let mut writer =
            StreamWriter::try_new(&mut buf, batch.schema().as_ref()).map_err(|e| e.to_string())?;
        writer.write(&batch).map_err(|e| e.to_string())?;
        writer.finish().map_err(|e| e.to_string())?;
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Array;

    fn one_column(values: Vec<Option<Value>>) -> RecordBatch {
        let mut rs = ResultSet::new(vec!["c".into()]);
        for v in values {
            rs.push_row(vec![v]);
        }
        to_record_batch(&rs).unwrap()
    }

    #[test]
    fn a_result_with_no_columns_encodes() {
        // What a schema statement (`CREATE INDEX …`) answers with.
        let rs = ResultSet::new(Vec::new());
        let batch = to_record_batch(&rs).unwrap();
        assert_eq!((batch.num_columns(), batch.num_rows()), (0, 0));
        assert!(!to_ipc_bytes(&rs).unwrap().is_empty());
    }

    #[test]
    fn test_arrow_date32_mapping() {
        // Value::Date(19737) is 2024-01-15 (days since epoch).
        let v = Value::Date(19737_i32);
        assert!(matches!(observe(ColKind::Empty, &v), ColKind::Date));
        let b = one_column(vec![Some(v), None]);
        assert_eq!(b.schema().field(0).data_type(), &DataType::Date32);
        let a = b.column(0).as_any().downcast_ref::<Date32Array>().unwrap();
        assert_eq!(a.value(0), 19737);
        assert!(a.is_null(1));
    }

    #[test]
    fn test_arrow_binary_mapping() {
        let v = Value::Binary(vec![1, 2, 3]);
        assert!(matches!(observe(ColKind::Empty, &v), ColKind::Binary));
        let b = one_column(vec![Some(v), Some(Value::Binary(vec![])), None]);
        assert_eq!(b.schema().field(0).data_type(), &DataType::LargeBinary);
        let a = b
            .column(0)
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .unwrap();
        assert_eq!(a.value(0), &[1, 2, 3]);
        assert_eq!(a.value(1), &[] as &[u8]);
        assert!(a.is_null(2));
    }

    #[test]
    fn test_arrow_local_datetime_mapping() {
        let b = one_column(vec![Some(Value::LocalDateTime(-5)), None]);
        assert_eq!(
            b.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, None)
        );
        let a = b
            .column(0)
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(a.value(0), -5);
    }

    #[test]
    fn test_arrow_datetime_mixed_offsets_become_utc_instants() {
        // Same instant written with two offsets: the column is UTC and both
        // cells are the identical UTC microseconds (offsets are dropped).
        let utc = 1_700_000_000_000_000_i64;
        let b = one_column(vec![
            Some(Value::DateTime {
                micros_utc: utc,
                offset_seconds: 0,
            }),
            Some(Value::DateTime {
                micros_utc: utc,
                offset_seconds: -5 * 3600,
            }),
        ]);
        assert_eq!(
            b.schema().field(0).data_type(),
            &DataType::Timestamp(TimeUnit::Microsecond, Some("+00:00".into()))
        );
        let a = b
            .column(0)
            .as_any()
            .downcast_ref::<TimestampMicrosecondArray>()
            .unwrap();
        assert_eq!(a.value(0), utc);
        assert_eq!(a.value(1), utc);
    }

    #[test]
    fn test_arrow_duration_is_lossy_documented() {
        let b = one_column(vec![
            Some(Value::Duration {
                months: 0,
                days: 2,
                nanos: 1_500,
            }),
            // sub-microsecond nanos truncated
            Some(Value::Duration {
                months: 0,
                days: 0,
                nanos: 999,
            }),
            Some(Value::Duration {
                months: 0,
                days: -1,
                nanos: 0,
            }),
            None,
        ]);
        assert_eq!(
            b.schema().field(0).data_type(),
            &DataType::Duration(TimeUnit::Microsecond)
        );
        let a = b
            .column(0)
            .as_any()
            .downcast_ref::<DurationMicrosecondArray>()
            .unwrap();
        assert_eq!(a.value(0), 2 * 86_400_000_000 + 1);
        assert_eq!(a.value(1), 0);
        assert_eq!(a.value(2), -86_400_000_000);
        assert!(a.is_null(3));
    }

    fn dur(months: i32, days: i32, nanos: i64) -> Value {
        Value::Duration {
            months,
            days,
            nanos,
        }
    }

    #[test]
    fn test_arrow_duration_extremes_saturate() {
        let b = one_column(vec![
            Some(dur(0, i32::MAX, i64::MAX)),
            Some(dur(0, i32::MIN, i64::MIN)),
        ]);
        let a = b
            .column(0)
            .as_any()
            .downcast_ref::<DurationMicrosecondArray>()
            .unwrap();
        assert_eq!(a.value(0), i64::MAX);
        assert_eq!(a.value(1), i64::MIN);
    }

    #[test]
    fn test_arrow_duration_with_months_is_text() {
        let b = one_column(vec![Some(dur(1, 0, 0))]);
        assert_eq!(b.schema().field(0).data_type(), &DataType::Utf8);
        let a = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(a.value(0), "P1M");
    }

    #[test]
    fn test_arrow_month_free_duration_stays_duration() {
        let b = one_column(vec![Some(dur(0, 2, 0))]);
        assert_eq!(
            b.schema().field(0).data_type(),
            &DataType::Duration(TimeUnit::Microsecond)
        );
    }

    #[test]
    fn test_arrow_one_month_duration_makes_whole_column_text() {
        let b = one_column(vec![Some(dur(0, 2, 0)), Some(dur(1, 0, 0)), None]);
        assert_eq!(b.schema().field(0).data_type(), &DataType::Utf8);
        let a = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(a.value(0), "P2D");
        assert_eq!(a.value(1), "P1M");
        assert!(a.is_null(2));
    }

    #[test]
    fn test_arrow_mixed_v11_column_falls_back_to_text() {
        let b = one_column(vec![Some(Value::Date(19737)), Some(Value::Int(1))]);
        assert_eq!(b.schema().field(0).data_type(), &DataType::Utf8);
        let a = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(a.value(0), "2024-01-15");
        let b = one_column(vec![Some(Value::Date(0)), Some(Value::LocalDateTime(0))]);
        assert_eq!(b.schema().field(0).data_type(), &DataType::Utf8);
    }

    #[test]
    fn test_arrow_v11_ipc_roundtrip_bytes_nonempty() {
        let mut rs = ResultSet::new(vec!["b".into(), "d".into()]);
        rs.push_row(vec![Some(Value::Binary(vec![9])), Some(Value::Date(1))]);
        assert!(!to_ipc_bytes(&rs).unwrap().is_empty());
    }
}
