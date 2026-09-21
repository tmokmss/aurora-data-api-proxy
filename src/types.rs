//! Turning Data API values back into PostgreSQL ones.
//!
//! The Data API does not return PostgreSQL values; it returns JSON that has
//! already lost some of the original shape. `numeric` arrives as a string,
//! `bytea` as base64, and -- the trap that matters most -- `timestamptz`
//! arrives as a UTC wall-clock string with the zone offset *stripped*. Handing
//! that last one to a client unchanged makes it read the value in its own
//! session time zone, which is a wrong answer delivered without an error.
//!
//! What we do have is `columnMetadata.typeName`, which carries the real
//! PostgreSQL type name. That is the anchor: from it we recover the type OID,
//! render the value in PostgreSQL's own text format, and encode binary output
//! for the clients that ask for it.
//!
//! Types we cannot encode faithfully are reported to the client as `text`
//! rather than mislabelled. A client then sees a readable value, or a clean
//! type error if it wanted something else -- never a corrupted one.

use aws_sdk_rdsdata::types::{ArrayValue, Field};
use pgwire::api::Type;

/// A value could not be converted, with the reason to report to the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvError(pub String);

impl std::fmt::Display for ConvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConvError {}

type Result<T> = std::result::Result<T, ConvError>;

/// Map a Data API `typeName` to the PostgreSQL type the proxy will advertise.
///
/// Anything the proxy cannot render faithfully in both text and binary maps to
/// [`Type::TEXT`], so the client gets a readable value rather than a value
/// mislabelled as a type it will decode wrongly.
pub fn map_type(type_name: &str) -> Type {
    // Data API names array types the way PostgreSQL does internally, with a
    // leading underscore: `_int4` is `int4[]`.
    if let Some(elem) = type_name.strip_prefix('_') {
        return array_of(elem);
    }
    match type_name {
        "bool" => Type::BOOL,
        // The Data API reports a column's *declared* type for the serial
        // family, which are not real types: `serial` is an `int4` with a
        // sequence default. Left unmapped, an `id serial` column would be
        // delivered as text while the client, told `int4` by `Describe`,
        // decoded four bytes that were not there.
        "int2" | "smallserial" => Type::INT2,
        "int4" | "serial" => Type::INT4,
        "int8" | "bigserial" => Type::INT8,
        "float4" => Type::FLOAT4,
        "float8" => Type::FLOAT8,
        "numeric" => Type::NUMERIC,
        "text" => Type::TEXT,
        "varchar" => Type::VARCHAR,
        "bpchar" => Type::BPCHAR,
        "name" => Type::NAME,
        "oid" => Type::OID,
        "date" => Type::DATE,
        "time" => Type::TIME,
        "timestamp" => Type::TIMESTAMP,
        "timestamptz" => Type::TIMESTAMPTZ,
        "uuid" => Type::UUID,
        "json" => Type::JSON,
        "jsonb" => Type::JSONB,
        "bytea" => Type::BYTEA,
        // Everything else -- inet, interval, xml, tsvector, enums, composites,
        // ranges, PostGIS -- is reported as text. See the module docs.
        _ => Type::TEXT,
    }
}

/// The array type whose element type is named `elem`.
fn array_of(elem: &str) -> Type {
    match elem {
        "bool" => Type::BOOL_ARRAY,
        "int2" => Type::INT2_ARRAY,
        "int4" => Type::INT4_ARRAY,
        "int8" => Type::INT8_ARRAY,
        "float4" => Type::FLOAT4_ARRAY,
        "float8" => Type::FLOAT8_ARRAY,
        "numeric" => Type::NUMERIC_ARRAY,
        "text" => Type::TEXT_ARRAY,
        "varchar" => Type::VARCHAR_ARRAY,
        "bpchar" => Type::BPCHAR_ARRAY,
        "name" => Type::NAME_ARRAY,
        "oid" => Type::OID_ARRAY,
        "date" => Type::DATE_ARRAY,
        "time" => Type::TIME_ARRAY,
        "timestamp" => Type::TIMESTAMP_ARRAY,
        "timestamptz" => Type::TIMESTAMPTZ_ARRAY,
        "uuid" => Type::UUID_ARRAY,
        "json" => Type::JSON_ARRAY,
        "jsonb" => Type::JSONB_ARRAY,
        "bytea" => Type::BYTEA_ARRAY,
        _ => Type::TEXT_ARRAY,
    }
}

/// How much to give up on when retrying a query the Data API would not return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastScope {
    /// Only the types the Data API refuses outright.
    Unreturnable,
    /// Also dates and timestamps, which fail only for particular *values*.
    AlsoTemporal,
}

/// The cast that would make a column of this type returnable, if it needs one.
///
/// The Data API refuses to return some types at all -- `interval`, `"char"`,
/// `xml`, `regtype[]` -- with `UnsupportedResultException`, and a single such
/// column fails the whole query. That is what breaks `\d` in psql, whose
/// catalogue queries select `"char"` columns.
///
/// A column this returns a cast for under [`CastScope::Unreturnable`] is one
/// the proxy would have reported to the client as `text` in any case, so
/// casting it server-side costs nothing.
///
/// [`CastScope::AlsoTemporal`] gives up more: a `timestamptz` holding
/// `infinity` makes the Data API fail with a bare `InternalFailure` and no
/// message at all, and there is no way to know from the metadata whether a
/// column holds one. That is why `\du` fails -- the `postgres` role's
/// `rolvaliduntil` is `infinity`. Casting the column to text loses its type but
/// returns the data, which beats failing.
pub fn needs_text_cast(type_name: &str, scope: CastScope) -> Option<&'static str> {
    if let Some(elem) = type_name.strip_prefix('_') {
        if elem != "text" && array_of(elem) == Type::TEXT_ARRAY {
            return Some("text[]");
        }
        if scope == CastScope::AlsoTemporal && is_temporal(elem) {
            return Some("text[]");
        }
        return None;
    }
    if type_name != "text" && map_type(type_name) == Type::TEXT {
        return Some("text");
    }
    if scope == CastScope::AlsoTemporal && is_temporal(type_name) {
        return Some("text");
    }
    None
}

fn is_temporal(type_name: &str) -> bool {
    matches!(type_name, "date" | "timestamp" | "timestamptz")
}

/// The element type of an array type, if `ty` is one.
pub fn element_of(ty: &Type) -> Option<Type> {
    match *ty {
        Type::BOOL_ARRAY => Some(Type::BOOL),
        Type::INT2_ARRAY => Some(Type::INT2),
        Type::INT4_ARRAY => Some(Type::INT4),
        Type::INT8_ARRAY => Some(Type::INT8),
        Type::FLOAT4_ARRAY => Some(Type::FLOAT4),
        Type::FLOAT8_ARRAY => Some(Type::FLOAT8),
        Type::NUMERIC_ARRAY => Some(Type::NUMERIC),
        Type::TEXT_ARRAY => Some(Type::TEXT),
        Type::VARCHAR_ARRAY => Some(Type::VARCHAR),
        Type::BPCHAR_ARRAY => Some(Type::BPCHAR),
        Type::NAME_ARRAY => Some(Type::NAME),
        Type::CHAR_ARRAY => Some(Type::CHAR),
        Type::OID_ARRAY => Some(Type::OID),
        Type::DATE_ARRAY => Some(Type::DATE),
        Type::TIME_ARRAY => Some(Type::TIME),
        Type::TIMESTAMP_ARRAY => Some(Type::TIMESTAMP),
        Type::TIMESTAMPTZ_ARRAY => Some(Type::TIMESTAMPTZ),
        Type::UUID_ARRAY => Some(Type::UUID),
        Type::JSON_ARRAY => Some(Type::JSON),
        Type::JSONB_ARRAY => Some(Type::JSONB),
        Type::BYTEA_ARRAY => Some(Type::BYTEA),
        _ => None,
    }
}

/// `pg_type.typlen`: a type's fixed width, or a negative value if it varies.
pub fn type_size(ty: &Type) -> i16 {
    match *ty {
        Type::BOOL | Type::CHAR => 1,
        Type::INT2 => 2,
        Type::INT4 | Type::FLOAT4 | Type::DATE | Type::OID => 4,
        Type::INT8 | Type::FLOAT8 | Type::TIME | Type::TIMESTAMP | Type::TIMESTAMPTZ => 8,
        Type::UUID => 16,
        _ => -1,
    }
}

/// The largest length a `varchar(n)`/`char(n)` may declare.
const MAX_STRING_TYPMOD: i32 = 10_485_760;

/// `pg_attribute.atttypmod` for a column, from the Data API's precision/scale.
///
/// Clients derive display precision from this, so a wrong value is visible:
/// JDBC renders a `numeric` with a tail of spurious zeros when the scale is
/// nonsense. When the metadata does not describe a real modifier -- an
/// unconstrained `numeric`, or a `text` column whose precision is reported as
/// `i32::MAX` -- we return `-1`, PostgreSQL's encoding for "no modifier".
pub fn type_modifier(ty: &Type, precision: i32, scale: i32) -> i32 {
    match *ty {
        Type::NUMERIC => {
            // A declared numeric(p, s) has 1 <= p <= 1000 and 0 <= s <= p.
            if (1..=1000).contains(&precision) && (0..=precision).contains(&scale) {
                ((precision << 16) | scale) + 4
            } else {
                -1
            }
        }
        Type::VARCHAR | Type::BPCHAR if (1..=MAX_STRING_TYPMOD).contains(&precision) => {
            precision + 4
        }
        _ => -1,
    }
}

/// Render a Data API value in PostgreSQL's text format.
///
/// `None` is SQL NULL.
pub fn field_to_text(field: &Field, ty: &Type) -> Result<Option<String>> {
    match field {
        Field::IsNull(true) => Ok(None),
        Field::IsNull(false) => Err(ConvError(
            "the Data API returned isNull=false, which carries no value".to_string(),
        )),
        Field::BooleanValue(b) => Ok(Some(if *b { "t" } else { "f" }.to_string())),
        Field::LongValue(n) => Ok(Some(n.to_string())),
        Field::DoubleValue(d) => Ok(Some(render_float(*d, ty))),
        Field::StringValue(s) => Ok(Some(render_string(s, ty))),
        Field::BlobValue(b) => Ok(Some(render_bytea(b.as_ref()))),
        Field::ArrayValue(a) => {
            let elem = element_of(ty).unwrap_or(Type::TEXT);
            Ok(Some(array_to_text(a, &elem)?))
        }
        other => Err(ConvError(format!(
            "the Data API returned a value variant this proxy does not understand: {other:?}"
        ))),
    }
}

/// PostgreSQL's text rendering of a float.
///
/// Rust's default formatting is the shortest form that round-trips, which is
/// what PostgreSQL emits too under the default `extra_float_digits`.
fn render_float(d: f64, ty: &Type) -> String {
    if d.is_nan() {
        return "NaN".to_string();
    }
    if d.is_infinite() {
        return if d > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    // A float4 arrives widened to f64; narrowing it back before formatting
    // avoids printing the artefacts of that widening (0.1f32 as f64 is
    // 0.10000000149011612).
    if *ty == Type::FLOAT4 {
        let f = d as f32;
        if f.is_finite() {
            return format!("{f}");
        }
    }
    format!("{d}")
}

/// Fix up a string value that the Data API has flattened.
fn render_string(s: &str, ty: &Type) -> String {
    if *ty == Type::TIMESTAMPTZ {
        return with_utc_offset(s);
    }
    s.to_string()
}

/// Append the UTC offset the Data API drops from `timestamptz` values.
///
/// Data API sessions always run with `TimeZone=UTC` (verified against the live
/// service), and it returns `timestamptz` as a bare wall-clock string. A client
/// reading that without an offset applies its own zone and silently shifts the
/// value, so we restore the `+00` the server would have sent.
fn with_utc_offset(s: &str) -> String {
    // `infinity` and `-infinity` have no offset, and a value that already
    // carries one needs no help.
    let t = s.trim();
    if t.eq_ignore_ascii_case("infinity") || t.eq_ignore_ascii_case("-infinity") {
        return s.to_string();
    }
    if has_zone_suffix(t) {
        return s.to_string();
    }
    format!("{s}+00")
}

/// Whether a timestamp string already ends in a zone offset such as `+09` or
/// `-05:30`.
fn has_zone_suffix(s: &str) -> bool {
    // Scan back over an offset of the form [+-]HH[:MM[:SS]]; the date part
    // itself contains `-`, so a bare `-` is not enough to decide.
    let b = s.as_bytes();
    let Some(sign) = b.iter().rposition(|&c| c == b'+' || c == b'-') else {
        return false;
    };
    // The date's hyphens all sit before the time, which starts at the space.
    let time_start = s.find(' ').map(|i| i + 1).unwrap_or(0);
    if sign < time_start {
        return false;
    }
    b[sign + 1..]
        .iter()
        .all(|&c| c.is_ascii_digit() || c == b':')
        && sign + 1 < b.len()
}

/// PostgreSQL's text rendering of a `bytea`: `\x` followed by lowercase hex.
fn render_bytea(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Render a Data API array as a PostgreSQL array literal.
fn array_to_text(array: &ArrayValue, elem: &Type) -> Result<String> {
    let mut out = String::from("{");
    let mut first = true;
    let mut push = |item: Option<String>, out: &mut String| {
        if !first {
            out.push(',');
        }
        first = false;
        match item {
            None => out.push_str("NULL"),
            Some(s) => out.push_str(&quote_array_element(&s)),
        }
    };

    match array {
        ArrayValue::BooleanValues(v) => {
            for item in v {
                push(
                    item.map(|b| if b { "t" } else { "f" }.to_string()),
                    &mut out,
                );
            }
        }
        ArrayValue::LongValues(v) => {
            for item in v {
                push(item.map(|n| n.to_string()), &mut out);
            }
        }
        ArrayValue::DoubleValues(v) => {
            for item in v {
                push(item.map(|d| render_float(d, elem)), &mut out);
            }
        }
        ArrayValue::StringValues(v) => {
            for item in v {
                push(item.as_ref().map(|s| render_string(s, elem)), &mut out);
            }
        }
        ArrayValue::ArrayValues(v) => {
            for item in v {
                // A nested array is itself a literal, and must not be quoted.
                if !first {
                    out.push(',');
                }
                first = false;
                match item {
                    None => out.push_str("NULL"),
                    Some(inner) => out.push_str(&array_to_text(inner, elem)?),
                }
            }
        }
        other => {
            return Err(ConvError(format!(
                "the Data API returned an array variant this proxy does not understand: {other:?}"
            )));
        }
    }
    out.push('}');
    Ok(out)
}

/// Quote one element of a PostgreSQL array literal, if it needs quoting.
fn quote_array_element(s: &str) -> String {
    let needs_quotes = s.is_empty()
        || s.eq_ignore_ascii_case("null")
        || s.chars()
            .any(|c| c.is_whitespace() || matches!(c, '{' | '}' | ',' | '"' | '\\'));
    if !needs_quotes {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Binary encoding
// ---------------------------------------------------------------------------

/// Microseconds from the Unix epoch to PostgreSQL's, 2000-01-01.
const PG_EPOCH_UNIX_SECS: i64 = 946_684_800;

/// Encode a value, given in PostgreSQL text format, in PostgreSQL's binary
/// format for `ty`.
///
/// Clients that ask for binary results get this; clients that ask for text get
/// the input unchanged. Only the types [`map_type`] is willing to advertise
/// reach this function, so an unsupported type here is a bug rather than a
/// user-visible condition.
pub fn text_to_binary(text: &str, ty: &Type) -> Result<Vec<u8>> {
    if let Some(elem) = element_of(ty) {
        return array_to_binary(text, &elem);
    }
    match *ty {
        Type::BOOL => Ok(vec![u8::from(text == "t" || text == "true")]),
        Type::INT2 => Ok(parse::<i16>(text, "int2")?.to_be_bytes().to_vec()),
        Type::INT4 => Ok(parse::<i32>(text, "int4")?.to_be_bytes().to_vec()),
        Type::INT8 => Ok(parse::<i64>(text, "int8")?.to_be_bytes().to_vec()),
        Type::OID => Ok(parse::<u32>(text, "oid")?.to_be_bytes().to_vec()),
        Type::FLOAT4 => Ok(parse_float32(text)?.to_be_bytes().to_vec()),
        Type::FLOAT8 => Ok(parse_float64(text)?.to_be_bytes().to_vec()),
        Type::NUMERIC => numeric_to_binary(text),
        Type::UUID => uuid_to_binary(text),
        Type::BYTEA => bytea_to_binary(text),
        Type::DATE => date_to_binary(text),
        Type::TIME => Ok(time_to_micros(text)?.to_be_bytes().to_vec()),
        Type::TIMESTAMP | Type::TIMESTAMPTZ => timestamp_to_binary(text, ty),
        // jsonb's binary format is a version byte followed by the text.
        Type::JSONB => {
            let mut out = Vec::with_capacity(text.len() + 1);
            out.push(1u8);
            out.extend_from_slice(text.as_bytes());
            Ok(out)
        }
        // text, varchar, bpchar, name, char, json, xml: the bytes as they are.
        _ => Ok(text.as_bytes().to_vec()),
    }
}

fn parse<T: std::str::FromStr>(text: &str, what: &str) -> Result<T> {
    text.trim()
        .parse::<T>()
        .map_err(|_| ConvError(format!("cannot read {text:?} as {what}")))
}

fn parse_float32(text: &str) -> Result<f32> {
    match text {
        "NaN" => Ok(f32::NAN),
        "Infinity" => Ok(f32::INFINITY),
        "-Infinity" => Ok(f32::NEG_INFINITY),
        _ => parse::<f32>(text, "float4"),
    }
}

fn parse_float64(text: &str) -> Result<f64> {
    match text {
        "NaN" => Ok(f64::NAN),
        "Infinity" => Ok(f64::INFINITY),
        "-Infinity" => Ok(f64::NEG_INFINITY),
        _ => parse::<f64>(text, "float8"),
    }
}

fn uuid_to_binary(text: &str) -> Result<Vec<u8>> {
    let hex: Vec<u8> = text.bytes().filter(|b| *b != b'-').collect::<Vec<_>>();
    if hex.len() != 32 {
        return Err(ConvError(format!("cannot read {text:?} as uuid")));
    }
    let mut out = Vec::with_capacity(16);
    for pair in hex.chunks(2) {
        let s = std::str::from_utf8(pair).map_err(|_| ConvError("invalid uuid".into()))?;
        out.push(
            u8::from_str_radix(s, 16)
                .map_err(|_| ConvError(format!("cannot read {text:?} as uuid")))?,
        );
    }
    Ok(out)
}

fn bytea_to_binary(text: &str) -> Result<Vec<u8>> {
    let hex = text
        .strip_prefix("\\x")
        .ok_or_else(|| ConvError(format!("cannot read {text:?} as bytea")))?;
    if hex.len() % 2 != 0 {
        return Err(ConvError(format!("cannot read {text:?} as bytea")));
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(hex.len() / 2);
    for pair in bytes.chunks(2) {
        let s = std::str::from_utf8(pair).map_err(|_| ConvError("invalid bytea".into()))?;
        out.push(
            u8::from_str_radix(s, 16)
                .map_err(|_| ConvError(format!("cannot read {text:?} as bytea")))?,
        );
    }
    Ok(out)
}

fn date_to_binary(text: &str) -> Result<Vec<u8>> {
    match text {
        "infinity" => return Ok(i32::MAX.to_be_bytes().to_vec()),
        "-infinity" => return Ok(i32::MIN.to_be_bytes().to_vec()),
        _ => {}
    }
    let date = chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .map_err(|_| ConvError(format!("cannot read {text:?} as date")))?;
    let pg_epoch = chrono::NaiveDate::from_ymd_opt(2000, 1, 1).expect("2000-01-01 is a valid date");
    let days = (date - pg_epoch).num_days();
    Ok(i32::try_from(days)
        .map_err(|_| ConvError(format!("date {text:?} is out of range")))?
        .to_be_bytes()
        .to_vec())
}

fn time_to_micros(text: &str) -> Result<i64> {
    let t = chrono::NaiveTime::parse_from_str(text, "%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveTime::parse_from_str(text, "%H:%M:%S"))
        .map_err(|_| ConvError(format!("cannot read {text:?} as time")))?;
    use chrono::Timelike;
    Ok(i64::from(t.num_seconds_from_midnight()) * 1_000_000 + i64::from(t.nanosecond()) / 1_000)
}

fn timestamp_to_binary(text: &str, ty: &Type) -> Result<Vec<u8>> {
    match text {
        "infinity" => return Ok(i64::MAX.to_be_bytes().to_vec()),
        "-infinity" => return Ok(i64::MIN.to_be_bytes().to_vec()),
        _ => {}
    }
    // `field_to_text` appends `+00` to timestamptz; strip it back off, since
    // the value is already UTC and binary carries no zone.
    let base = if *ty == Type::TIMESTAMPTZ {
        text.strip_suffix("+00").unwrap_or(text)
    } else {
        text
    };
    let dt = chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%d %H:%M:%S"))
        .map_err(|_| ConvError(format!("cannot read {base:?} as timestamp")))?;
    let micros = dt
        .and_utc()
        .timestamp_micros()
        .checked_sub(PG_EPOCH_UNIX_SECS * 1_000_000)
        .ok_or_else(|| ConvError(format!("timestamp {base:?} is out of range")))?;
    Ok(micros.to_be_bytes().to_vec())
}

/// Encode a `numeric` in PostgreSQL's binary format.
///
/// The wire form is a sign, a base-10000 digit vector, the weight of its
/// leading group and the display scale.
fn numeric_to_binary(text: &str) -> Result<Vec<u8>> {
    const SIGN_POS: u16 = 0x0000;
    const SIGN_NEG: u16 = 0x4000;
    const SIGN_NAN: u16 = 0xC000;
    const SIGN_PINF: u16 = 0xD000;
    const SIGN_NINF: u16 = 0xF000;

    let mut out = Vec::with_capacity(16);
    let write_header = |ndigits: i16, weight: i16, sign: u16, dscale: i16, out: &mut Vec<u8>| {
        out.extend_from_slice(&ndigits.to_be_bytes());
        out.extend_from_slice(&weight.to_be_bytes());
        out.extend_from_slice(&sign.to_be_bytes());
        out.extend_from_slice(&dscale.to_be_bytes());
    };

    let t = text.trim();
    match t {
        "NaN" => {
            write_header(0, 0, SIGN_NAN, 0, &mut out);
            return Ok(out);
        }
        "Infinity" => {
            write_header(0, 0, SIGN_PINF, 0, &mut out);
            return Ok(out);
        }
        "-Infinity" => {
            write_header(0, 0, SIGN_NINF, 0, &mut out);
            return Ok(out);
        }
        _ => {}
    }

    let (sign, rest) = match t.strip_prefix('-') {
        Some(r) => (SIGN_NEG, r),
        None => (SIGN_POS, t.strip_prefix('+').unwrap_or(t)),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((i, f)) => (i, f),
        None => (rest, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(ConvError(format!("cannot read {text:?} as numeric")));
    }
    if !int_part
        .bytes()
        .chain(frac_part.bytes())
        .all(|b| b.is_ascii_digit())
    {
        return Err(ConvError(format!("cannot read {text:?} as numeric")));
    }
    let dscale = i16::try_from(frac_part.len())
        .map_err(|_| ConvError(format!("numeric {text:?} has too many decimal places")))?;

    // Group into base-10000 digits, aligned on the decimal point: the integer
    // part pads on the left, the fraction on the right.
    let int_pad = (4 - int_part.len() % 4) % 4;
    let padded_int = "0".repeat(int_pad) + int_part;
    let frac_pad = (4 - frac_part.len() % 4) % 4;
    let padded_frac = frac_part.to_string() + &"0".repeat(frac_pad);

    let mut digits: Vec<i16> = Vec::new();
    for chunk in padded_int.as_bytes().chunks(4) {
        digits.push(chunk_to_digit(chunk)?);
    }
    for chunk in padded_frac.as_bytes().chunks(4) {
        digits.push(chunk_to_digit(chunk)?);
    }

    // `weight` counts base-10000 groups before the point, less one.
    let int_groups = (padded_int.len() / 4) as i32;
    let mut weight = int_groups - 1;

    // Leading zero groups shift the weight; trailing ones are simply dropped.
    let lead = digits.iter().take_while(|d| **d == 0).count();
    digits.drain(..lead);
    weight -= lead as i32;
    while digits.last() == Some(&0) {
        digits.pop();
    }
    if digits.is_empty() {
        // A zero has no digits at all, and a weight of zero.
        weight = 0;
    }

    write_header(
        i16::try_from(digits.len()).map_err(|_| ConvError("numeric is too long".into()))?,
        i16::try_from(weight).map_err(|_| ConvError("numeric is out of range".into()))?,
        sign,
        dscale,
        &mut out,
    );
    for d in digits {
        out.extend_from_slice(&d.to_be_bytes());
    }
    Ok(out)
}

fn chunk_to_digit(chunk: &[u8]) -> Result<i16> {
    let s = std::str::from_utf8(chunk).map_err(|_| ConvError("invalid numeric".into()))?;
    s.parse::<i16>()
        .map_err(|_| ConvError(format!("invalid numeric group {s:?}")))
}

/// Encode a PostgreSQL array literal in binary format.
fn array_to_binary(text: &str, elem: &Type) -> Result<Vec<u8>> {
    let items = parse_array_literal(text)?;
    let mut out = Vec::new();
    let has_null = items.iter().any(Option::is_none);
    // One dimension only: the Data API cannot return multidimensional arrays
    // at all, so there is nothing else to encode.
    out.extend_from_slice(&1i32.to_be_bytes());
    out.extend_from_slice(&i32::from(has_null).to_be_bytes());
    out.extend_from_slice(&elem.oid().to_be_bytes());
    out.extend_from_slice(&i32::try_from(items.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(&1i32.to_be_bytes()); // lower bound
    for item in items {
        match item {
            None => out.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(s) => {
                let bytes = text_to_binary(&s, elem)?;
                out.extend_from_slice(
                    &i32::try_from(bytes.len())
                        .map_err(|_| ConvError("array element is too large".into()))?
                        .to_be_bytes(),
                );
                out.extend_from_slice(&bytes);
            }
        }
    }
    Ok(out)
}

/// Split a one-dimensional PostgreSQL array literal into its elements.
///
/// An unquoted `NULL` is the SQL null; a quoted `"NULL"` is the string.
pub fn parse_array_literal(text: &str) -> Result<Vec<Option<String>>> {
    let t = text.trim();
    let inner = t
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .ok_or_else(|| ConvError(format!("cannot read {text:?} as an array")))?;
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    let mut items = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' if !quoted => {
                quoted = true;
                was_quoted = true;
            }
            '"' if quoted => quoted = false,
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            ',' if !quoted => {
                items.push(finish_array_element(&current, was_quoted));
                current.clear();
                was_quoted = false;
            }
            _ => current.push(c),
        }
    }
    items.push(finish_array_element(&current, was_quoted));
    Ok(items)
}

fn finish_array_element(s: &str, was_quoted: bool) -> Option<String> {
    if !was_quoted && s.eq_ignore_ascii_case("null") {
        None
    } else if was_quoted {
        Some(s.to_string())
    } else {
        Some(s.trim().to_string())
    }
}

/// Decode a value in PostgreSQL's binary format into its text format.
///
/// This is the inverse of [`text_to_binary`], and exists for bound parameters:
/// most drivers send those in binary, while the Data API wants them as JSON
/// scalars. Going via text keeps one canonical representation in the middle.
pub fn binary_to_text(bytes: &[u8], ty: &Type) -> Result<String> {
    if let Some(elem) = element_of(ty) {
        return binary_array_to_text(bytes, &elem);
    }
    match *ty {
        Type::BOOL => Ok(if bytes.first().copied().unwrap_or(0) == 0 {
            "f"
        } else {
            "t"
        }
        .to_string()),
        Type::INT2 => Ok(i16::from_be_bytes(fixed(bytes, "int2")?).to_string()),
        Type::INT4 => Ok(i32::from_be_bytes(fixed(bytes, "int4")?).to_string()),
        Type::INT8 => Ok(i64::from_be_bytes(fixed(bytes, "int8")?).to_string()),
        Type::OID => Ok(u32::from_be_bytes(fixed(bytes, "oid")?).to_string()),
        Type::FLOAT4 => Ok(render_float(
            f32::from_be_bytes(fixed(bytes, "float4")?) as f64,
            &Type::FLOAT4,
        )),
        Type::FLOAT8 => Ok(render_float(
            f64::from_be_bytes(fixed(bytes, "float8")?),
            &Type::FLOAT8,
        )),
        Type::NUMERIC => binary_numeric_to_text(bytes),
        Type::UUID => {
            if bytes.len() != 16 {
                return Err(ConvError("a binary uuid must be 16 bytes".into()));
            }
            let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            Ok(format!(
                "{}-{}-{}-{}-{}",
                &h[0..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..32]
            ))
        }
        Type::BYTEA => Ok(render_bytea(bytes)),
        Type::DATE => {
            let days = i32::from_be_bytes(fixed(bytes, "date")?);
            match days {
                i32::MAX => Ok("infinity".to_string()),
                i32::MIN => Ok("-infinity".to_string()),
                _ => {
                    let epoch = chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
                        .expect("2000-01-01 is a valid date");
                    let date = epoch
                        .checked_add_signed(chrono::Duration::days(i64::from(days)))
                        .ok_or_else(|| ConvError("date is out of range".into()))?;
                    Ok(date.format("%Y-%m-%d").to_string())
                }
            }
        }
        Type::TIME => {
            let micros = i64::from_be_bytes(fixed(bytes, "time")?);
            let secs = micros.div_euclid(1_000_000);
            let frac = micros.rem_euclid(1_000_000);
            let t = format!(
                "{:02}:{:02}:{:02}",
                secs / 3600,
                (secs / 60) % 60,
                secs % 60
            );
            Ok(if frac == 0 {
                t
            } else {
                format!("{t}.{:06}", frac).trim_end_matches('0').to_string()
            })
        }
        Type::TIMESTAMP | Type::TIMESTAMPTZ => {
            let micros = i64::from_be_bytes(fixed(bytes, "timestamp")?);
            match micros {
                i64::MAX => return Ok("infinity".to_string()),
                i64::MIN => return Ok("-infinity".to_string()),
                _ => {}
            }
            let unix_micros = micros
                .checked_add(PG_EPOCH_UNIX_SECS * 1_000_000)
                .ok_or_else(|| ConvError("timestamp is out of range".into()))?;
            let dt = chrono::DateTime::from_timestamp_micros(unix_micros)
                .ok_or_else(|| ConvError("timestamp is out of range".into()))?;
            let naive = dt.naive_utc();
            let s = if naive.and_utc().timestamp_subsec_micros() == 0 {
                naive.format("%Y-%m-%d %H:%M:%S").to_string()
            } else {
                naive
                    .format("%Y-%m-%d %H:%M:%S%.6f")
                    .to_string()
                    .trim_end_matches('0')
                    .to_string()
            };
            Ok(s)
        }
        Type::JSONB => {
            // A leading version byte, then the JSON text.
            let body = bytes.strip_prefix(&[1u8]).unwrap_or(bytes);
            Ok(String::from_utf8_lossy(body).into_owned())
        }
        _ => Ok(String::from_utf8_lossy(bytes).into_owned()),
    }
}

fn fixed<const N: usize>(bytes: &[u8], what: &str) -> Result<[u8; N]> {
    bytes.try_into().map_err(|_| {
        ConvError(format!(
            "a binary {what} must be {N} bytes, got {}",
            bytes.len()
        ))
    })
}

/// Decode PostgreSQL's binary `numeric` into its text form.
fn binary_numeric_to_text(bytes: &[u8]) -> Result<String> {
    if bytes.len() < 8 {
        return Err(ConvError("a binary numeric needs at least 8 bytes".into()));
    }
    let ndigits = i16::from_be_bytes([bytes[0], bytes[1]]) as usize;
    let weight = i16::from_be_bytes([bytes[2], bytes[3]]) as i32;
    let sign = u16::from_be_bytes([bytes[4], bytes[5]]);
    let dscale = i16::from_be_bytes([bytes[6], bytes[7]]) as usize;

    match sign {
        0xC000 => return Ok("NaN".to_string()),
        0xD000 => return Ok("Infinity".to_string()),
        0xF000 => return Ok("-Infinity".to_string()),
        _ => {}
    }
    if bytes.len() < 8 + ndigits * 2 {
        return Err(ConvError(
            "a binary numeric is shorter than its digit count".into(),
        ));
    }
    let digits: Vec<i16> = (0..ndigits)
        .map(|i| i16::from_be_bytes([bytes[8 + i * 2], bytes[9 + i * 2]]))
        .collect();

    let mut int_part = String::new();
    if weight >= 0 {
        for i in 0..=weight {
            let d = digits.get(i as usize).copied().unwrap_or(0);
            if i == 0 {
                int_part.push_str(&d.to_string());
            } else {
                int_part.push_str(&format!("{d:04}"));
            }
        }
    }
    if int_part.is_empty() {
        int_part.push('0');
    }

    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    out.push_str(&int_part);

    if dscale > 0 {
        let mut frac = String::new();
        let mut i = weight + 1;
        while frac.len() < dscale {
            let d = if i < 0 {
                0
            } else {
                digits.get(i as usize).copied().unwrap_or(0)
            };
            frac.push_str(&format!("{d:04}"));
            i += 1;
        }
        frac.truncate(dscale);
        out.push('.');
        out.push_str(&frac);
    }
    Ok(out)
}

/// Decode a binary array into a PostgreSQL array literal.
fn binary_array_to_text(bytes: &[u8], elem: &Type) -> Result<String> {
    if bytes.len() < 12 {
        return Err(ConvError("a binary array needs at least 12 bytes".into()));
    }
    let ndim = i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if ndim == 0 {
        return Ok("{}".to_string());
    }
    if ndim != 1 {
        return Err(ConvError(
            "the Aurora Data API cannot carry multidimensional arrays".into(),
        ));
    }
    let len = i32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]) as usize;
    let mut pos = 20; // header + one dimension
    let mut items: Vec<Option<String>> = Vec::with_capacity(len);
    for _ in 0..len {
        if pos + 4 > bytes.len() {
            return Err(ConvError("a binary array ended early".into()));
        }
        let n = i32::from_be_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
        pos += 4;
        if n < 0 {
            items.push(None);
            continue;
        }
        let n = n as usize;
        if pos + n > bytes.len() {
            return Err(ConvError("a binary array ended early".into()));
        }
        items.push(Some(binary_to_text(&bytes[pos..pos + n], elem)?));
        pos += n;
    }
    let mut out = String::from("{");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        match item {
            None => out.push_str("NULL"),
            Some(s) => out.push_str(&quote_array_element(s)),
        }
    }
    out.push('}');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_smithy_types::Blob;

    #[test]
    fn maps_scalar_type_names() {
        assert_eq!(map_type("int4"), Type::INT4);
        assert_eq!(map_type("timestamptz"), Type::TIMESTAMPTZ);
        assert_eq!(map_type("jsonb"), Type::JSONB);
    }

    #[test]
    fn maps_the_declared_names_the_data_api_uses_for_serial_columns() {
        // Verified against the live service: an `id serial` column comes back
        // as typeName "serial", not "int4".
        assert_eq!(map_type("serial"), Type::INT4);
        assert_eq!(map_type("bigserial"), Type::INT8);
        assert_eq!(map_type("smallserial"), Type::INT2);
    }

    #[test]
    fn maps_array_type_names() {
        assert_eq!(map_type("_int4"), Type::INT4_ARRAY);
        assert_eq!(map_type("_text"), Type::TEXT_ARRAY);
        assert_eq!(element_of(&Type::INT4_ARRAY), Some(Type::INT4));
    }

    #[test]
    fn falls_back_to_text_for_types_it_cannot_encode() {
        // Reported as text rather than mislabelled -- see the module docs.
        assert_eq!(map_type("interval"), Type::TEXT);
        assert_eq!(map_type("inet"), Type::TEXT);
        assert_eq!(map_type("my_enum"), Type::TEXT);
        assert_eq!(map_type("_inet"), Type::TEXT_ARRAY);
        // `"char"` is pg_catalog's one-byte type, which the Data API refuses
        // to return at all. Claiming to support it would break psql's `\d`.
        assert_eq!(map_type("char"), Type::TEXT);
    }

    #[test]
    fn asks_for_a_cast_on_types_the_data_api_will_not_return() {
        use CastScope::Unreturnable as U;
        assert_eq!(needs_text_cast("char", U), Some("text"));
        assert_eq!(needs_text_cast("interval", U), Some("text"));
        assert_eq!(needs_text_cast("inet", U), Some("text"));
        assert_eq!(needs_text_cast("xml", U), Some("text"));
        assert_eq!(needs_text_cast("_regtype", U), Some("text[]"));
        // Types that come back cleanly must be left alone.
        assert_eq!(needs_text_cast("text", U), None);
        assert_eq!(needs_text_cast("int4", U), None);
        assert_eq!(needs_text_cast("name", U), None);
        assert_eq!(needs_text_cast("_text", U), None);
        assert_eq!(needs_text_cast("_int4", U), None);
        // A timestamp is only a problem for particular values, so it is spared
        // until the wider retry.
        assert_eq!(needs_text_cast("timestamptz", U), None);
    }

    #[test]
    fn gives_up_on_temporal_types_only_in_the_wider_retry() {
        use CastScope::AlsoTemporal as T;
        assert_eq!(needs_text_cast("timestamptz", T), Some("text"));
        assert_eq!(needs_text_cast("timestamp", T), Some("text"));
        assert_eq!(needs_text_cast("date", T), Some("text"));
        assert_eq!(needs_text_cast("_timestamptz", T), Some("text[]"));
        assert_eq!(needs_text_cast("int4", T), None);
        assert_eq!(needs_text_cast("text", T), None);
    }

    #[test]
    fn renders_booleans_the_way_postgres_does() {
        let t = field_to_text(&Field::BooleanValue(true), &Type::BOOL).unwrap();
        assert_eq!(t.as_deref(), Some("t"));
        let f = field_to_text(&Field::BooleanValue(false), &Type::BOOL).unwrap();
        assert_eq!(f.as_deref(), Some("f"));
    }

    #[test]
    fn renders_null() {
        assert_eq!(
            field_to_text(&Field::IsNull(true), &Type::TEXT).unwrap(),
            None
        );
    }

    #[test]
    fn renders_bytea_as_hex() {
        let v = field_to_text(
            &Field::BlobValue(Blob::new(vec![0xde, 0xad, 0xbe, 0xef])),
            &Type::BYTEA,
        )
        .unwrap();
        assert_eq!(v.as_deref(), Some("\\xdeadbeef"));
    }

    #[test]
    fn restores_the_utc_offset_on_timestamptz() {
        // The Data API drops the offset; without this the client would read
        // the value in its own zone and get a different instant.
        let v = field_to_text(
            &Field::StringValue("2024-01-15 03:34:56.789".into()),
            &Type::TIMESTAMPTZ,
        )
        .unwrap();
        assert_eq!(v.as_deref(), Some("2024-01-15 03:34:56.789+00"));
    }

    #[test]
    fn leaves_plain_timestamps_alone() {
        let v = field_to_text(
            &Field::StringValue("2024-01-15 03:34:56.789".into()),
            &Type::TIMESTAMP,
        )
        .unwrap();
        assert_eq!(v.as_deref(), Some("2024-01-15 03:34:56.789"));
    }

    #[test]
    fn does_not_double_up_an_existing_offset() {
        let v = field_to_text(
            &Field::StringValue("2024-01-15 03:34:56+02".into()),
            &Type::TIMESTAMPTZ,
        )
        .unwrap();
        assert_eq!(v.as_deref(), Some("2024-01-15 03:34:56+02"));
    }

    #[test]
    fn leaves_infinite_timestamps_alone() {
        let v = field_to_text(&Field::StringValue("infinity".into()), &Type::TIMESTAMPTZ).unwrap();
        assert_eq!(v.as_deref(), Some("infinity"));
    }

    #[test]
    fn narrows_float4_before_rendering() {
        // 0.1f32 widened to f64 is 0.10000000149011612; PostgreSQL prints 0.1.
        let v = field_to_text(&Field::DoubleValue(0.1f32 as f64), &Type::FLOAT4).unwrap();
        assert_eq!(v.as_deref(), Some("0.1"));
    }

    #[test]
    fn renders_arrays_as_literals() {
        let a = ArrayValue::LongValues(vec![Some(1), Some(2), None]);
        let v = field_to_text(&Field::ArrayValue(a), &Type::INT4_ARRAY).unwrap();
        assert_eq!(v.as_deref(), Some("{1,2,NULL}"));
    }

    #[test]
    fn quotes_array_elements_that_need_it() {
        let a = ArrayValue::StringValues(vec![
            Some("plain".into()),
            Some("has space".into()),
            Some("has\"quote".into()),
            Some("NULL".into()),
            Some(String::new()),
            None,
        ]);
        let v = field_to_text(&Field::ArrayValue(a), &Type::TEXT_ARRAY)
            .unwrap()
            .unwrap();
        assert_eq!(v, r#"{plain,"has space","has\"quote","NULL","",NULL}"#);
    }

    #[test]
    fn array_literals_round_trip() {
        let parsed = parse_array_literal(r#"{plain,"has space","NULL",NULL,""}"#).unwrap();
        assert_eq!(
            parsed,
            vec![
                Some("plain".to_string()),
                Some("has space".to_string()),
                Some("NULL".to_string()),
                None,
                Some(String::new()),
            ]
        );
        assert_eq!(
            parse_array_literal("{}").unwrap(),
            Vec::<Option<String>>::new()
        );
    }

    #[test]
    fn encodes_integers_in_binary() {
        assert_eq!(text_to_binary("1", &Type::INT2).unwrap(), vec![0, 1]);
        assert_eq!(text_to_binary("1", &Type::INT4).unwrap(), vec![0, 0, 0, 1]);
        assert_eq!(text_to_binary("-1", &Type::INT8).unwrap(), vec![0xff; 8]);
    }

    #[test]
    fn encodes_bool_in_binary() {
        assert_eq!(text_to_binary("t", &Type::BOOL).unwrap(), vec![1]);
        assert_eq!(text_to_binary("f", &Type::BOOL).unwrap(), vec![0]);
    }

    #[test]
    fn encodes_uuid_in_binary() {
        let b = text_to_binary("550e8400-e29b-41d4-a716-446655440000", &Type::UUID).unwrap();
        assert_eq!(b.len(), 16);
        assert_eq!(b[0], 0x55);
        assert_eq!(b[15], 0x00);
    }

    #[test]
    fn encodes_bytea_in_binary() {
        assert_eq!(
            text_to_binary("\\xdeadbeef", &Type::BYTEA).unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn encodes_jsonb_with_its_version_byte() {
        let b = text_to_binary("{\"a\":1}", &Type::JSONB).unwrap();
        assert_eq!(b[0], 1);
        assert_eq!(&b[1..], b"{\"a\":1}");
    }

    #[test]
    fn encodes_dates_relative_to_the_postgres_epoch() {
        assert_eq!(
            text_to_binary("2000-01-01", &Type::DATE).unwrap(),
            0i32.to_be_bytes().to_vec()
        );
        assert_eq!(
            text_to_binary("2000-01-02", &Type::DATE).unwrap(),
            1i32.to_be_bytes().to_vec()
        );
    }

    #[test]
    fn encodes_times_as_microseconds() {
        assert_eq!(
            text_to_binary("00:00:01", &Type::TIME).unwrap(),
            1_000_000i64.to_be_bytes().to_vec()
        );
        assert_eq!(
            text_to_binary("12:34:56.789", &Type::TIME).unwrap(),
            ((12 * 3600 + 34 * 60 + 56) * 1_000_000i64 + 789_000)
                .to_be_bytes()
                .to_vec()
        );
    }

    #[test]
    fn encodes_timestamps_relative_to_the_postgres_epoch() {
        assert_eq!(
            text_to_binary("2000-01-01 00:00:00", &Type::TIMESTAMP).unwrap(),
            0i64.to_be_bytes().to_vec()
        );
        // The `+00` this proxy appends must not derail parsing.
        assert_eq!(
            text_to_binary("2000-01-01 00:00:01+00", &Type::TIMESTAMPTZ).unwrap(),
            1_000_000i64.to_be_bytes().to_vec()
        );
    }

    /// Expected encodings were taken from PostgreSQL's own `numeric_send`.
    #[test]
    fn encodes_numerics_in_binary() {
        // 0 -> no digits at all.
        assert_eq!(
            text_to_binary("0", &Type::NUMERIC).unwrap(),
            vec![0, 0, 0, 0, 0, 0, 0, 0]
        );
        // 1 -> ndigits=1 weight=0 sign=+ dscale=0 digits=[1]
        assert_eq!(
            text_to_binary("1", &Type::NUMERIC).unwrap(),
            vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 1]
        );
        // -1.5 -> ndigits=2 weight=0 sign=0x4000 dscale=1 digits=[1, 5000]
        assert_eq!(
            text_to_binary("-1.5", &Type::NUMERIC).unwrap(),
            vec![0, 2, 0, 0, 0x40, 0, 0, 1, 0, 1, 0x13, 0x88]
        );
        // 12345.6789 -> ndigits=3 weight=1 dscale=4 digits=[1, 2345, 6789]
        assert_eq!(
            text_to_binary("12345.6789", &Type::NUMERIC).unwrap(),
            vec![0, 3, 0, 1, 0, 0, 0, 4, 0, 1, 0x09, 0x29, 0x1a, 0x85]
        );
    }

    #[test]
    fn encodes_numeric_nan() {
        assert_eq!(
            text_to_binary("NaN", &Type::NUMERIC).unwrap(),
            vec![0, 0, 0, 0, 0xC0, 0, 0, 0]
        );
    }

    #[test]
    fn rejects_values_it_cannot_encode() {
        assert!(text_to_binary("not-a-number", &Type::INT4).is_err());
        assert!(text_to_binary("not-a-uuid", &Type::UUID).is_err());
        assert!(text_to_binary("12.x", &Type::NUMERIC).is_err());
    }

    #[test]
    fn encodes_arrays_in_binary() {
        let b = text_to_binary("{1,2}", &Type::INT4_ARRAY).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(&1i32.to_be_bytes()); // ndim
        expected.extend_from_slice(&0i32.to_be_bytes()); // has_null
        expected.extend_from_slice(&Type::INT4.oid().to_be_bytes());
        expected.extend_from_slice(&2i32.to_be_bytes()); // length
        expected.extend_from_slice(&1i32.to_be_bytes()); // lower bound
        expected.extend_from_slice(&4i32.to_be_bytes());
        expected.extend_from_slice(&1i32.to_be_bytes());
        expected.extend_from_slice(&4i32.to_be_bytes());
        expected.extend_from_slice(&2i32.to_be_bytes());
        assert_eq!(b, expected);
    }

    #[test]
    fn computes_type_modifiers() {
        // numeric(18, 4) packs as ((18 << 16) | 4) + 4.
        assert_eq!(type_modifier(&Type::NUMERIC, 18, 4), ((18 << 16) | 4) + 4);
        assert_eq!(type_modifier(&Type::VARCHAR, 10, 0), 14);
        // An unconstrained text column reports i32::MAX; that is not a modifier.
        assert_eq!(type_modifier(&Type::VARCHAR, i32::MAX, 0), -1);
        assert_eq!(type_modifier(&Type::NUMERIC, i32::MAX, 0), -1);
        assert_eq!(type_modifier(&Type::INT4, 10, 0), -1);
    }

    /// Every value a client can send must survive the trip out to binary and
    /// back, or a bound parameter silently changes on its way to the cluster.
    #[track_caller]
    fn round_trip(text: &str, ty: &Type) {
        let binary = text_to_binary(text, ty).expect("encodes");
        let back = binary_to_text(&binary, ty).expect("decodes");
        assert_eq!(back, text, "round trip failed for {ty:?}");
    }

    #[test]
    fn binary_round_trips_scalars() {
        round_trip("t", &Type::BOOL);
        round_trip("f", &Type::BOOL);
        round_trip("0", &Type::INT2);
        round_trip("-32768", &Type::INT2);
        round_trip("2147483647", &Type::INT4);
        round_trip("-9223372036854775808", &Type::INT8);
        round_trip("4294967295", &Type::OID);
        round_trip("0.1", &Type::FLOAT4);
        round_trip("3.141592653589793", &Type::FLOAT8);
        round_trip("550e8400-e29b-41d4-a716-446655440000", &Type::UUID);
        round_trip("\\xdeadbeef", &Type::BYTEA);
        round_trip("hello", &Type::TEXT);
        round_trip("{\"a\": 1}", &Type::JSONB);
    }

    #[test]
    fn binary_round_trips_dates_and_times() {
        round_trip("2024-01-15", &Type::DATE);
        round_trip("infinity", &Type::DATE);
        round_trip("-infinity", &Type::DATE);
        round_trip("12:34:56", &Type::TIME);
        round_trip("12:34:56.789", &Type::TIME);
        round_trip("2024-01-15 12:34:56", &Type::TIMESTAMP);
        round_trip("2024-01-15 12:34:56.789", &Type::TIMESTAMP);
        round_trip("infinity", &Type::TIMESTAMP);
    }

    #[test]
    fn binary_round_trips_numerics() {
        round_trip("0", &Type::NUMERIC);
        round_trip("1", &Type::NUMERIC);
        round_trip("-1.5", &Type::NUMERIC);
        round_trip("12345.6789", &Type::NUMERIC);
        round_trip("123456789.123456", &Type::NUMERIC);
        round_trip("0.0001", &Type::NUMERIC);
        round_trip("-0.5", &Type::NUMERIC);
        round_trip("NaN", &Type::NUMERIC);
        round_trip("1000000000000000000000", &Type::NUMERIC);
    }

    #[test]
    fn binary_round_trips_arrays() {
        round_trip("{1,2,3}", &Type::INT4_ARRAY);
        round_trip("{}", &Type::INT4_ARRAY);
        round_trip("{1,NULL,3}", &Type::INT4_ARRAY);
        round_trip("{a,b}", &Type::TEXT_ARRAY);
        round_trip(r#"{"has space","has\"quote"}"#, &Type::TEXT_ARRAY);
    }

    #[test]
    fn a_timestamptz_parameter_round_trips_without_its_offset() {
        // `field_to_text` appends `+00` on the way out; a parameter coming back
        // in binary carries no zone, so the text form has none either.
        let binary = text_to_binary("2024-01-15 12:34:56+00", &Type::TIMESTAMPTZ).unwrap();
        assert_eq!(
            binary_to_text(&binary, &Type::TIMESTAMPTZ).unwrap(),
            "2024-01-15 12:34:56"
        );
    }

    #[test]
    fn rejects_binary_values_of_the_wrong_width() {
        assert!(binary_to_text(&[0, 1], &Type::INT4).is_err());
        assert!(binary_to_text(&[0; 4], &Type::UUID).is_err());
        assert!(binary_to_text(&[0; 2], &Type::NUMERIC).is_err());
    }

    #[test]
    fn rejects_multidimensional_arrays() {
        let mut b = Vec::new();
        b.extend_from_slice(&2i32.to_be_bytes()); // ndim
        b.extend_from_slice(&0i32.to_be_bytes());
        b.extend_from_slice(&Type::INT4.oid().to_be_bytes());
        b.extend_from_slice(&[0u8; 8]);
        let err = binary_to_text(&b, &Type::INT4_ARRAY).unwrap_err();
        assert!(err.0.contains("multidimensional"));
    }
}
