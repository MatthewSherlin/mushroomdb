//! Bolt request and response messages.
//!
//! A message is a PackStream structure whose tag names the message. This module
//! decodes the requests the connection actor handles itself (`HELLO`,
//! `GOODBYE`, `RESET`) and carries the rest as raw fields for the handlers that
//! own them. Responses are encoded through [`ServerMessage::encode`], which
//! refuses anything the wire format cannot carry rather than emit a corrupt
//! frame.

pub mod hello;
pub mod reset;
pub mod route;
pub mod run;

use crate::error::BoltError;
use crate::packstream::{self, PackStruct, PackValue};

pub const TAG_HELLO: u8 = 0x01;
pub const TAG_GOODBYE: u8 = 0x02;
pub const TAG_RESET: u8 = 0x0F;
pub const TAG_RUN: u8 = 0x10;
pub const TAG_BEGIN: u8 = 0x11;
pub const TAG_COMMIT: u8 = 0x12;
pub const TAG_ROLLBACK: u8 = 0x13;
pub const TAG_DISCARD: u8 = 0x2F;
pub const TAG_PULL: u8 = 0x3F;
pub const TAG_ROUTE: u8 = 0x66;

pub const TAG_SUCCESS: u8 = 0x70;
pub const TAG_RECORD: u8 = 0x71;
pub const TAG_IGNORED: u8 = 0x7E;
pub const TAG_FAILURE: u8 = 0x7F;

/// Most bytes (or elements) one PackStream string, byte array, list or map can
/// hold: its size field is 32 bits wide.
pub const MAX_PACK_LEN: u64 = u32::MAX as u64;

/// Most fields a PackStream structure can hold: its size field is 16 bits wide.
pub const MAX_STRUCT_FIELDS: usize = u16::MAX as usize;

/// Requests the connection actor does not handle itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Run,
    Begin,
    Commit,
    Rollback,
    Discard,
    Pull,
    Route,
}

/// A decoded client request. `Debug` shows no field values: a request's fields
/// can hold credentials or query parameters.
#[derive(Clone, PartialEq)]
pub enum ClientMessage {
    Hello(hello::Hello),
    Goodbye,
    Reset,
    /// A request handled by a later layer, with its fields undecoded.
    Other(MessageKind, Vec<PackValue>),
}

impl std::fmt::Debug for ClientMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientMessage::Hello(hello) => f.debug_tuple("Hello").field(hello).finish(),
            ClientMessage::Goodbye => f.write_str("Goodbye"),
            ClientMessage::Reset => f.write_str("Reset"),
            ClientMessage::Other(kind, fields) => write!(f, "{kind:?}({} fields)", fields.len()),
        }
    }
}

impl ClientMessage {
    /// Decodes one deframed message.
    pub fn decode(bytes: &[u8]) -> Result<Self, BoltError> {
        let (value, used) = packstream::unpack(bytes)
            .map_err(|e| BoltError::Invalid(format!("malformed message: {e}")))?;
        if used != bytes.len() {
            return Err(BoltError::Invalid(
                "malformed message: bytes after the structure".into(),
            ));
        }
        let PackValue::Struct(PackStruct { tag, fields }) = value else {
            return Err(BoltError::Invalid("a message must be a structure".into()));
        };
        let expect_no_fields = |fields: &[PackValue], name: &str| {
            if fields.is_empty() {
                Ok(())
            } else {
                Err(BoltError::Invalid(format!("{name} takes no fields")))
            }
        };
        let kind = match tag {
            TAG_HELLO => return hello::Hello::from_fields(fields).map(ClientMessage::Hello),
            TAG_GOODBYE => {
                expect_no_fields(&fields, "GOODBYE")?;
                return Ok(ClientMessage::Goodbye);
            }
            TAG_RESET => {
                expect_no_fields(&fields, "RESET")?;
                return Ok(ClientMessage::Reset);
            }
            TAG_RUN => MessageKind::Run,
            TAG_BEGIN => MessageKind::Begin,
            TAG_COMMIT => MessageKind::Commit,
            TAG_ROLLBACK => MessageKind::Rollback,
            TAG_DISCARD => MessageKind::Discard,
            TAG_PULL => MessageKind::Pull,
            TAG_ROUTE => MessageKind::Route,
            other => {
                return Err(BoltError::Invalid(format!(
                    "unknown message tag 0x{other:02X}"
                )))
            }
        };
        Ok(ClientMessage::Other(kind, fields))
    }
}

/// A response the connection sends.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Success(Vec<(String, PackValue)>),
    /// One row of a result: the values, in field order.
    Record(Vec<PackValue>),
    Failure(BoltError),
    Ignored,
}

impl ServerMessage {
    /// Encodes the message, or explains why the wire format cannot carry it.
    ///
    /// `pack` writes a size header with a bare cast, so a value over the
    /// header's range would truncate silently into a frame that decodes as
    /// something else. The check runs first.
    pub fn encode(&self) -> Result<Vec<u8>, BoltError> {
        self.encode_with_limit(MAX_PACK_LEN)
    }

    fn encode_with_limit(&self, max_len: u64) -> Result<Vec<u8>, BoltError> {
        let value = self.to_pack();
        check_encodable(&value, max_len)?;
        let mut buf = Vec::new();
        packstream::pack(&value, &mut buf);
        Ok(buf)
    }

    /// The structure this message travels as.
    pub fn to_pack(&self) -> PackValue {
        match self {
            ServerMessage::Success(meta) => PackValue::Struct(PackStruct::new(
                TAG_SUCCESS,
                vec![PackValue::Map(meta.clone())],
            )),
            ServerMessage::Record(values) => run::record_struct(values.clone()),
            ServerMessage::Ignored => PackValue::Struct(PackStruct::new(TAG_IGNORED, vec![])),
            ServerMessage::Failure(err) => PackValue::Struct(PackStruct::new(
                TAG_FAILURE,
                vec![PackValue::Map(vec![
                    ("code".into(), PackValue::String(err.code())),
                    ("message".into(), PackValue::String(err.message())),
                ])],
            )),
        }
    }

    /// An encoded `FAILURE` for an error that cannot be encoded as-is: the
    /// message text is replaced by a fixed one.
    pub fn encode_or_failure(&self) -> Vec<u8> {
        match self.encode() {
            Ok(bytes) => bytes,
            Err(err) => ServerMessage::Failure(BoltError::TooLarge(err.message()))
                .encode()
                .unwrap_or_default(),
        }
    }
}

/// Refuses a value `pack` would encode wrongly: a string, byte array, list or
/// map longer than `max_len`, or a structure with more than
/// [`MAX_STRUCT_FIELDS`] fields. Also bounds nesting at
/// [`packstream::MAX_DEPTH`], the depth `unpack` accepts.
pub fn check_encodable(value: &PackValue, max_len: u64) -> Result<(), BoltError> {
    check_depth(value, max_len, 0)
}

fn too_large(what: &str) -> BoltError {
    BoltError::TooLarge(format!(
        "result value cannot be sent over Bolt: {what} exceeds the PackStream size limit"
    ))
}

fn check_depth(value: &PackValue, max_len: u64, depth: usize) -> Result<(), BoltError> {
    if depth > packstream::MAX_DEPTH {
        return Err(too_large("nesting"));
    }
    match value {
        PackValue::Bytes(b) if b.len() as u64 > max_len => Err(too_large("a byte array")),
        PackValue::String(s) if s.len() as u64 > max_len => Err(too_large("a string")),
        PackValue::List(items) => {
            if items.len() as u64 > max_len {
                return Err(too_large("a list"));
            }
            items
                .iter()
                .try_for_each(|v| check_depth(v, max_len, depth + 1))
        }
        PackValue::Map(entries) => {
            if entries.len() as u64 > max_len {
                return Err(too_large("a map"));
            }
            for (key, v) in entries {
                if key.len() as u64 > max_len {
                    return Err(too_large("a map key"));
                }
                check_depth(v, max_len, depth + 1)?;
            }
            Ok(())
        }
        PackValue::Struct(s) => {
            if s.fields.len() > MAX_STRUCT_FIELDS {
                return Err(too_large("a structure"));
            }
            s.fields
                .iter()
                .try_for_each(|v| check_depth(v, max_len, depth + 1))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_over_65535_fields_is_refused_not_truncated() {
        let big = PackValue::Struct(PackStruct::new(
            TAG_RECORD_FOR_TEST,
            vec![PackValue::Null; MAX_STRUCT_FIELDS + 1],
        ));
        assert!(matches!(
            check_encodable(&big, MAX_PACK_LEN),
            Err(BoltError::TooLarge(_))
        ));
        let ok = PackValue::Struct(PackStruct::new(
            TAG_RECORD_FOR_TEST,
            vec![PackValue::Null; MAX_STRUCT_FIELDS],
        ));
        assert!(check_encodable(&ok, MAX_PACK_LEN).is_ok());
    }

    const TAG_RECORD_FOR_TEST: u8 = 0x71;

    #[test]
    fn oversized_string_is_refused_with_a_failure_not_a_corrupt_frame() {
        // The real limit is 4 GiB; the limit is a parameter so the test does
        // not allocate it.
        let msg = ServerMessage::Success(vec![("k".into(), PackValue::String("x".repeat(40)))]);
        assert!(msg.encode_with_limit(39).is_err());
        assert!(msg.encode_with_limit(40).is_ok());
        let oversized_list = PackValue::List(vec![PackValue::Null; 5]);
        assert!(check_encodable(&oversized_list, 4).is_err());
        let oversized_map = PackValue::Map(vec![("a".into(), PackValue::Null); 5]);
        assert!(check_encodable(&oversized_map, 4).is_err());
        let oversized_bytes = PackValue::Bytes(vec![0; 5]);
        assert!(check_encodable(&oversized_bytes, 4).is_err());
    }

    #[test]
    fn unencodable_response_becomes_a_failure_message() {
        let big = ServerMessage::Success(vec![(
            "k".into(),
            PackValue::Struct(PackStruct::new(
                TAG_RECORD_FOR_TEST,
                vec![PackValue::Null; MAX_STRUCT_FIELDS + 1],
            )),
        )]);
        let bytes = big.encode_or_failure();
        let (value, _) = packstream::unpack(&bytes).unwrap();
        let PackValue::Struct(s) = value else {
            panic!()
        };
        assert_eq!(s.tag, TAG_FAILURE);
    }

    #[test]
    fn client_message_debug_shows_no_field_values() {
        let msg = ClientMessage::Other(
            MessageKind::Run,
            vec![PackValue::String("secret-param".into())],
        );
        assert!(!format!("{msg:?}").contains("secret-param"));
    }

    #[test]
    fn decode_rejects_trailing_bytes_unknown_tags_and_non_structs() {
        let mut bytes = Vec::new();
        packstream::pack(
            &PackValue::Struct(PackStruct::new(TAG_RESET, vec![])),
            &mut bytes,
        );
        assert_eq!(ClientMessage::decode(&bytes), Ok(ClientMessage::Reset));
        bytes.push(0xC0);
        assert!(ClientMessage::decode(&bytes).is_err());
        let mut unknown = Vec::new();
        packstream::pack(
            &PackValue::Struct(PackStruct::new(0x55, vec![])),
            &mut unknown,
        );
        assert!(ClientMessage::decode(&unknown).is_err());
        let mut not_struct = Vec::new();
        packstream::pack(&PackValue::Int(1), &mut not_struct);
        assert!(ClientMessage::decode(&not_struct).is_err());
        // RESET with a field is malformed.
        let mut bad = Vec::new();
        packstream::pack(
            &PackValue::Struct(PackStruct::new(TAG_RESET, vec![PackValue::Null])),
            &mut bad,
        );
        assert!(ClientMessage::decode(&bad).is_err());
    }
}
