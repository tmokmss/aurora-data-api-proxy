//! Turning bound parameters into Data API values.
//!
//! A parameter arrives from the client as raw bytes in either text or binary
//! format, tagged with a PostgreSQL type OID. The Data API wants a JSON scalar.
//! The conversion goes through PostgreSQL's own text format as a common
//! currency, because that is the one representation both sides agree on.
//!
//! The type itself is carried separately, as a `CAST(:pN AS ...)` written into
//! the SQL -- see [`crate::sql::rewrite_placeholders_with_casts`] for why.

use aws_sdk_rdsdata::types::{Field, SqlParameter};
use aws_smithy_types::Blob;
use pgwire::api::Type;

use crate::sql::param_name;
use crate::types::{ConvError, binary_to_text};

/// The SQL type name to cast a parameter of type `ty` to, if we know one.
///
/// `unknown` is PostgreSQL's "the client did not say", and is not a type you
/// can cast to, so it yields `None` and the placeholder is left bare.
pub fn cast_name_for(ty: &Type) -> Option<String> {
    if *ty == Type::UNKNOWN {
        return None;
    }
    Some(ty.name().to_string())
}

/// Convert one bound parameter into a Data API `SqlParameter`.
///
/// `index` is 1-based, matching the `$n` it came from. `raw` is `None` for SQL
/// NULL.
pub fn to_sql_parameter(
    index: usize,
    raw: Option<&[u8]>,
    is_binary: bool,
    ty: &Type,
) -> Result<SqlParameter, ConvError> {
    let field = match raw {
        None => Field::IsNull(true),
        Some(bytes) => {
            // bytea travels as a blob rather than as `\x...` text: it is the one
            // type where the text form doubles the size for no benefit.
            if *ty == Type::BYTEA && !is_binary {
                Field::BlobValue(Blob::new(decode_bytea_text(bytes)?))
            } else if *ty == Type::BYTEA {
                Field::BlobValue(Blob::new(bytes.to_vec()))
            } else if is_binary {
                if *ty == Type::UNKNOWN {
                    return Err(ConvError(format!(
                        "parameter ${index} arrived in binary format with no type; the proxy \
                         cannot tell what those bytes mean"
                    )));
                }
                Field::StringValue(binary_to_text(bytes, ty)?)
            } else {
                Field::StringValue(
                    std::str::from_utf8(bytes)
                        .map_err(|_| {
                            ConvError(format!("parameter ${index} is not valid UTF-8 text"))
                        })?
                        .to_string(),
                )
            }
        }
    };

    Ok(SqlParameter::builder()
        .name(param_name(index))
        .value(field)
        .build())
}

/// Decode PostgreSQL's `\x...` hex text form of a `bytea`.
///
/// The older escape format (`\101\102`) is not accepted: no current client
/// sends it, and quietly mis-reading it would corrupt binary data.
fn decode_bytea_text(bytes: &[u8]) -> Result<Vec<u8>, ConvError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| ConvError("a bytea parameter is not valid text".to_string()))?;
    let hex = text.strip_prefix("\\x").ok_or_else(|| {
        ConvError("a bytea parameter must use the hex format, for example \\xdeadbeef".to_string())
    })?;
    if hex.len() % 2 != 0 {
        return Err(ConvError(
            "a bytea parameter has an odd number of hex digits".into(),
        ));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in hex.as_bytes().chunks(2) {
        let s = std::str::from_utf8(pair).map_err(|_| ConvError("invalid bytea".into()))?;
        out.push(
            u8::from_str_radix(s, 16)
                .map_err(|_| ConvError(format!("invalid hex digits {s:?} in a bytea parameter")))?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value_of(p: &SqlParameter) -> &Field {
        p.value.as_ref().expect("parameter has a value")
    }

    #[test]
    fn names_parameters_to_match_their_placeholders() {
        let p = to_sql_parameter(3, Some(b"x"), false, &Type::TEXT).unwrap();
        assert_eq!(p.name.as_deref(), Some("p3"));
    }

    #[test]
    fn sends_null_as_null() {
        let p = to_sql_parameter(1, None, false, &Type::INT4).unwrap();
        assert!(matches!(value_of(&p), Field::IsNull(true)));
    }

    #[test]
    fn sends_text_parameters_unchanged() {
        let p = to_sql_parameter(1, Some(b"hello"), false, &Type::TEXT).unwrap();
        match value_of(&p) {
            Field::StringValue(s) => assert_eq!(s, "hello"),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    #[test]
    fn decodes_binary_parameters_to_text() {
        let p = to_sql_parameter(1, Some(&42i32.to_be_bytes()), true, &Type::INT4).unwrap();
        match value_of(&p) {
            Field::StringValue(s) => assert_eq!(s, "42"),
            other => panic!("expected a string, got {other:?}"),
        }
    }

    #[test]
    fn sends_bytea_as_a_blob_from_either_format() {
        let from_text = to_sql_parameter(1, Some(b"\\xdeadbeef"), false, &Type::BYTEA).unwrap();
        let from_binary =
            to_sql_parameter(1, Some(&[0xde, 0xad, 0xbe, 0xef]), true, &Type::BYTEA).unwrap();
        for p in [&from_text, &from_binary] {
            match value_of(p) {
                Field::BlobValue(b) => assert_eq!(b.as_ref(), &[0xde, 0xad, 0xbe, 0xef]),
                other => panic!("expected a blob, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_a_bytea_in_the_old_escape_format() {
        // Silently mis-reading this would corrupt binary data.
        let err = to_sql_parameter(1, Some(b"\\101\\102"), false, &Type::BYTEA).unwrap_err();
        assert!(err.0.contains("hex format"));
    }

    #[test]
    fn refuses_binary_bytes_of_an_unknown_type() {
        let err = to_sql_parameter(1, Some(&[0, 1, 2, 3]), true, &Type::UNKNOWN).unwrap_err();
        assert!(err.0.contains("cannot tell what those bytes mean"));
    }

    #[test]
    fn names_cast_targets() {
        assert_eq!(cast_name_for(&Type::INT4).as_deref(), Some("int4"));
        assert_eq!(cast_name_for(&Type::TEXT_ARRAY).as_deref(), Some("_text"));
        assert_eq!(
            cast_name_for(&Type::TIMESTAMPTZ).as_deref(),
            Some("timestamptz")
        );
        // `unknown` is not something you can cast to.
        assert_eq!(cast_name_for(&Type::UNKNOWN), None);
    }
}
