//! Convert [`core_query::ResultSet`] to Arrow RecordBatches and IPC stream bytes.

use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_ipc::writer::StreamWriter;
use arrow_schema::{DataType, Field, Schema};
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
}

fn observe(kind: ColKind, v: &Value) -> ColKind {
    match (kind, v) {
        (ColKind::Empty | ColKind::Int, Value::Int(_)) => ColKind::Int,
        (ColKind::Empty | ColKind::Float, Value::Float(_)) => ColKind::Float,
        (ColKind::Int, Value::Float(_)) | (ColKind::Float, Value::Int(_)) => ColKind::Float,
        (ColKind::Empty | ColKind::Bool, Value::Bool(_)) => ColKind::Bool,
        (ColKind::Empty | ColKind::Str, Value::Str(_)) => ColKind::Str,
        (ColKind::Utf8, _) => ColKind::Utf8,
        (_, Value::List(_)) => ColKind::Utf8,
        _ => ColKind::Utf8,
    }
}

fn infer_column(rs: &ResultSet, col: usize) -> ColKind {
    let mut kind = ColKind::Empty;
    for i in 0..rs.len() {
        if let Some(v) = &rs.row(i)[col] {
            kind = observe(kind, v);
        }
    }
    kind
}

fn data_type(kind: ColKind) -> DataType {
    match kind {
        ColKind::Int => DataType::Int64,
        ColKind::Float => DataType::Float64,
        ColKind::Bool => DataType::Boolean,
        ColKind::Empty | ColKind::Str | ColKind::Utf8 => DataType::Utf8,
    }
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
                    rs.row(i)[col].as_ref().map(|v| match v {
                        Value::Int(n) => *n,
                        other => unreachable!("int column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(Int64Array::from(vals))
        }
        ColKind::Float => {
            let vals: Vec<Option<f64>> = (0..rs.len())
                .map(|i| rs.row(i)[col].as_ref().map(as_f64))
                .collect();
            Arc::new(Float64Array::from(vals))
        }
        ColKind::Bool => {
            let vals: Vec<Option<bool>> = (0..rs.len())
                .map(|i| {
                    rs.row(i)[col].as_ref().map(|v| match v {
                        Value::Bool(b) => *b,
                        other => unreachable!("bool column saw {other:?}"),
                    })
                })
                .collect();
            Arc::new(BooleanArray::from(vals))
        }
        ColKind::Empty | ColKind::Str | ColKind::Utf8 => {
            let vals: Vec<Option<String>> = (0..rs.len())
                .map(|i| rs.row(i)[col].as_ref().map(canonical_display))
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
    RecordBatch::try_new(schema, columns).map_err(|e| e.to_string())
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
