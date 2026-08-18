use arrow_array::{Array, BooleanArray, Float64Array, Int64Array, StringArray};
use arrow_bridge::{to_ipc_bytes, to_record_batch};
use arrow_ipc::reader::StreamReader;
use arrow_schema::DataType;
use core_query::{ResultSet, Value};
use std::io::Cursor;

fn cell(v: Value) -> Option<Value> {
    Some(v)
}

/// Binding: IPC STREAM roundtrip preserves every value, including nulls.
#[test]
fn ipc_roundtrip_preserves_values_and_nulls() {
    let mut rs = ResultSet::new(vec!["n".into(), "flag".into(), "label".into()]);
    rs.push_row(vec![
        cell(Value::Int(1)),
        cell(Value::Bool(true)),
        cell(Value::Str("a".into())),
    ]);
    rs.push_row(vec![None, cell(Value::Bool(false)), None]);
    rs.push_row(vec![
        cell(Value::Int(-7)),
        None,
        cell(Value::Str("z".into())),
    ]);

    let bytes = to_ipc_bytes(&rs).unwrap();
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).unwrap();
    let batch = reader.next().expect("one batch").unwrap();
    assert!(reader.next().is_none(), "single batch stream");

    assert_eq!(batch.num_rows(), 3);
    assert_eq!(batch.schema().fields().len(), 3);
    assert_eq!(batch.schema().field(0).name(), "n");
    assert_eq!(batch.schema().field(1).name(), "flag");
    assert_eq!(batch.schema().field(2).name(), "label");
    assert_eq!(batch.schema().field(0).data_type(), &DataType::Int64);
    assert_eq!(batch.schema().field(1).data_type(), &DataType::Boolean);
    assert_eq!(batch.schema().field(2).data_type(), &DataType::Utf8);

    let n = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(n.value(0), 1);
    assert!(n.is_null(1));
    assert_eq!(n.value(2), -7);

    let flag = batch
        .column(1)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .unwrap();
    assert!(flag.value(0));
    assert!(!flag.value(1));
    assert!(flag.is_null(2));

    let label = batch
        .column(2)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(label.value(0), "a");
    assert!(label.is_null(1));
    assert_eq!(label.value(2), "z");
}

/// Binding: per-column type inference policy is pinned.
#[test]
fn mixed_type_policy_is_pinned_per_column() {
    let mut rs = ResultSet::new(vec![
        "all_int".into(),
        "all_float".into(),
        "int_float".into(),
        "all_bool".into(),
        "all_str".into(),
        "lists".into(),
        "mixed".into(),
        "all_null".into(),
    ]);
    rs.push_row(vec![
        cell(Value::Int(1)),
        cell(Value::Float(1.5)),
        cell(Value::Int(2)),
        cell(Value::Bool(true)),
        cell(Value::Str("x".into())),
        cell(Value::List(vec![Value::Int(1), Value::Int(2)])),
        cell(Value::Int(9)),
        None,
    ]);
    rs.push_row(vec![
        cell(Value::Int(3)),
        cell(Value::Float(-0.25)),
        cell(Value::Float(2.5)),
        cell(Value::Bool(false)),
        cell(Value::Str("y".into())),
        cell(Value::List(vec![Value::Str("a".into())])),
        cell(Value::Str("nine".into())),
        None,
    ]);
    rs.push_row(vec![None, None, None, None, None, None, None, None]);

    let batch = to_record_batch(&rs).unwrap();
    let types: Vec<DataType> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.data_type().clone())
        .collect();
    assert_eq!(
        types,
        vec![
            DataType::Int64,
            DataType::Float64,
            DataType::Float64,
            DataType::Boolean,
            DataType::Utf8,
            DataType::Utf8,
            DataType::Utf8,
            DataType::Utf8,
        ]
    );

    let all_int = batch
        .column(0)
        .as_any()
        .downcast_ref::<Int64Array>()
        .unwrap();
    assert_eq!(all_int.value(0), 1);
    assert_eq!(all_int.value(1), 3);
    assert!(all_int.is_null(2));

    let all_float = batch
        .column(1)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    assert_eq!(all_float.value(0), 1.5);
    assert_eq!(all_float.value(1), -0.25);
    assert!(all_float.is_null(2));

    let mixed_num = batch
        .column(2)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    assert_eq!(mixed_num.value(0), 2.0);
    assert_eq!(mixed_num.value(1), 2.5);
    assert!(mixed_num.is_null(2));

    let lists = batch
        .column(5)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(lists.value(0), "[1, 2]");
    assert_eq!(lists.value(1), "[a]");
    assert!(lists.is_null(2));

    let mixed = batch
        .column(6)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(mixed.value(0), "9");
    assert_eq!(mixed.value(1), "nine");
    assert!(mixed.is_null(2));

    let all_null = batch
        .column(7)
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    assert_eq!(all_null.data_type(), &DataType::Utf8);
    assert!(all_null.is_null(0));
    assert!(all_null.is_null(1));
    assert!(all_null.is_null(2));
}

/// Binding: empty ResultSet yields an empty batch whose column names survive.
#[test]
fn empty_result_set_preserves_schema() {
    let rs = ResultSet::new(vec!["score".into(), "name".into()]);
    let batch = to_record_batch(&rs).unwrap();
    assert_eq!(batch.num_rows(), 0);
    assert_eq!(batch.schema().fields().len(), 2);
    assert_eq!(batch.schema().field(0).name(), "score");
    assert_eq!(batch.schema().field(1).name(), "name");
    assert_eq!(batch.schema().field(0).data_type(), &DataType::Utf8);
    assert_eq!(batch.schema().field(1).data_type(), &DataType::Utf8);

    let bytes = to_ipc_bytes(&rs).unwrap();
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None).unwrap();
    let ipc = reader.next().expect("one batch").unwrap();
    assert_eq!(ipc.num_rows(), 0);
    assert_eq!(ipc.schema().field(0).name(), "score");
    assert_eq!(ipc.schema().field(1).name(), "name");
    assert_eq!(ipc.schema().field(0).data_type(), &DataType::Utf8);
    assert_eq!(ipc.schema().field(1).data_type(), &DataType::Utf8);
}
