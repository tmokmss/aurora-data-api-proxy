//! Turning a Data API result into a PostgreSQL response.

use std::sync::Arc;

use aws_sdk_rdsdata::types::{ColumnMetadata, Field};
use bytes::{BufMut, BytesMut};
use pgwire::api::Type;
use pgwire::api::portal::Format;
use pgwire::api::results::{FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::error::ErrorInfo;
use pgwire::messages::data::DataRow;

use crate::sql;
use crate::types;

/// Build the result column descriptions from the Data API's column metadata.
pub fn build_fields(columns: &[ColumnMetadata], format: &Format) -> Vec<FieldInfo> {
    columns
        .iter()
        .enumerate()
        .map(|(i, col)| {
            let type_name = col.type_name.as_deref().unwrap_or("text");
            let ty = types::map_type(type_name);
            // `label` is what the query asked the column to be called; `name`
            // is the underlying column. PostgreSQL reports the label.
            let name = col
                .label
                .clone()
                .or_else(|| col.name.clone())
                .unwrap_or_else(|| format!("column{}", i + 1));
            FieldInfo::new(name, None, None, ty.clone(), format.format_for(i))
                .with_type_size(types::type_size(&ty))
                .with_type_modifier(types::type_modifier(&ty, col.precision, col.scale))
        })
        .collect()
}

/// Encode the Data API's records as PostgreSQL data rows.
pub fn encode_rows(
    records: &[Vec<Field>],
    fields: &[FieldInfo],
) -> Result<Vec<DataRow>, ErrorInfo> {
    let mut out = Vec::with_capacity(records.len());
    for record in records {
        let mut buf = BytesMut::with_capacity(64);
        for (i, field) in fields.iter().enumerate() {
            let value = match record.get(i) {
                Some(v) => v,
                // The service promised a column and did not send it; saying so
                // is better than quietly shifting every later column left.
                None => {
                    return Err(conversion_error(
                        field.name(),
                        &format!(
                            "the Data API returned {} values for {} columns",
                            record.len(),
                            fields.len()
                        ),
                    ));
                }
            };
            let text = types::field_to_text(value, field.datatype())
                .map_err(|e| conversion_error(field.name(), &e.0))?;
            match text {
                None => buf.put_i32(-1),
                Some(text) => {
                    let bytes = if field.format() == FieldFormat::Binary {
                        types::text_to_binary(&text, field.datatype())
                            .map_err(|e| conversion_error(field.name(), &e.0))?
                    } else {
                        text.into_bytes()
                    };
                    buf.put_i32(bytes.len() as i32);
                    buf.put_slice(&bytes);
                }
            }
        }
        out.push(DataRow::new(buf, fields.len() as i16));
    }
    Ok(out)
}

fn conversion_error(column: &str, detail: &str) -> ErrorInfo {
    let mut e = ErrorInfo::new(
        "ERROR".to_string(),
        // internal_error: the value reached us but could not be represented.
        "XX000".to_string(),
        format!("cannot convert column {column:?} to its PostgreSQL form: {detail}"),
    );
    e.hint = Some(
        "this is a gap in the proxy's type support; casting the column to text in the \
         query will get the value through"
            .to_string(),
    );
    e
}

/// Assemble a row-returning response.
pub fn rows_response(
    sql: &str,
    columns: &[ColumnMetadata],
    records: &[Vec<Field>],
    format: &Format,
) -> Result<Response, ErrorInfo> {
    let fields = build_fields(columns, format);
    let rows = encode_rows(records, &fields)?;
    let mut response = QueryResponse::new(
        Arc::new(fields),
        futures::stream::iter(rows.into_iter().map(Ok)),
    );
    response.set_command_tag(&row_tag_prefix(sql));
    Ok(Response::Query(response))
}

/// The command tag prefix for a statement that returns rows.
///
/// pgwire appends the row count, so this is the tag without it. `INSERT`
/// carries an OID slot that has been zero since PostgreSQL 12, which is why it
/// ends in `0`.
pub fn row_tag_prefix(sql: &str) -> String {
    match first_word(sql).as_str() {
        "INSERT" => "INSERT 0".to_string(),
        "UPDATE" => "UPDATE".to_string(),
        "DELETE" => "DELETE".to_string(),
        "MERGE" => "MERGE".to_string(),
        "WITH" | "" => "SELECT".to_string(),
        other => other.to_string(),
    }
}

/// The command tag for a statement that reports only a row count.
pub fn affected_tag(sql: &str, count: i64) -> Tag {
    let words = sql::leading_words(sql, 2);
    let verb = words.first().map(String::as_str).unwrap_or("");
    let count = count.max(0) as usize;
    match verb {
        "INSERT" => Tag::new("INSERT").with_oid(0).with_rows(count),
        "UPDATE" | "DELETE" | "MERGE" | "SELECT" | "COPY" | "FETCH" | "MOVE" => {
            Tag::new(verb).with_rows(count)
        }
        // Utility statements report the command, not a count. Most are two
        // words -- `CREATE TABLE`, `DROP INDEX`, `ALTER TABLE`.
        "CREATE" | "DROP" | "ALTER" | "GRANT" | "REVOKE" | "COMMENT" | "TRUNCATE" | "REFRESH"
        | "SECURITY" => match words.get(1) {
            Some(object) => Tag::new(&format!("{verb} {object}")),
            None => Tag::new(verb),
        },
        "" => Tag::new("SELECT").with_rows(0),
        other => Tag::new(other),
    }
}

fn first_word(sql: &str) -> String {
    sql::leading_words(sql, 1)
        .first()
        .cloned()
        .unwrap_or_default()
}

/// What a transaction-control statement asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxControl {
    Begin,
    Commit,
    Rollback,
}

/// Recognise the statements that must become Data API transaction calls.
///
/// `SAVEPOINT`, `RELEASE` and `ROLLBACK TO SAVEPOINT` are deliberately absent:
/// those work inside a Data API transaction exactly as written, so they are
/// forwarded unchanged.
pub fn transaction_control(sql: &str) -> Option<TxControl> {
    let words = sql::leading_words(sql, 2);
    let first = words.first()?.as_str();
    let second = words.get(1).map(String::as_str);
    match (first, second) {
        ("BEGIN", _) => Some(TxControl::Begin),
        ("START", Some("TRANSACTION")) => Some(TxControl::Begin),
        ("COMMIT", _) | ("END", _) => Some(TxControl::Commit),
        // `ROLLBACK TO [SAVEPOINT] x` rewinds within the transaction and must
        // not end it.
        ("ROLLBACK", Some("TO")) => None,
        ("ROLLBACK", _) | ("ABORT", _) => Some(TxControl::Rollback),
        _ => None,
    }
}

/// A `Type` for each result column, for the statements the proxy describes
/// without running them.
///
/// Each OID is put through the same filter as a type named in live column
/// metadata, so a column is described as exactly the type the proxy will go on
/// to send -- describing a column as `interval` and then delivering text would
/// be worse than calling it text from the start.
pub fn types_from_oids(oids: &[i64]) -> Vec<Type> {
    oids.iter()
        .map(
            |oid| match u32::try_from(*oid).ok().and_then(Type::from_oid) {
                Some(ty) => types::map_type(ty.name()),
                None => Type::TEXT,
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_row_returning_statements() {
        // pgwire appends the count, so `INSERT 0` becomes `INSERT 0 3`.
        assert_eq!(
            row_tag_prefix("INSERT INTO t VALUES (1) RETURNING id"),
            "INSERT 0"
        );
        assert_eq!(row_tag_prefix("SELECT 1"), "SELECT");
        assert_eq!(row_tag_prefix("update t set a=1 returning id"), "UPDATE");
        assert_eq!(
            row_tag_prefix("WITH x AS (SELECT 1) SELECT * FROM x"),
            "SELECT"
        );
    }

    #[test]
    fn tags_counted_statements() {
        use pgwire::messages::response::CommandComplete;
        let tag_of = |sql: &str, n: i64| {
            let cc: CommandComplete = affected_tag(sql, n).into();
            cc.tag
        };
        assert_eq!(tag_of("INSERT INTO t VALUES (1)", 1), "INSERT 0 1");
        assert_eq!(tag_of("UPDATE t SET a = 1", 3), "UPDATE 3");
        assert_eq!(tag_of("DELETE FROM t", 0), "DELETE 0");
        assert_eq!(tag_of("CREATE TABLE t (a int)", 0), "CREATE TABLE");
        assert_eq!(tag_of("drop index i", 0), "DROP INDEX");
        assert_eq!(tag_of("SET search_path = x", 0), "SET");
    }

    #[test]
    fn recognises_transaction_control() {
        assert_eq!(transaction_control("BEGIN"), Some(TxControl::Begin));
        assert_eq!(
            transaction_control("begin transaction"),
            Some(TxControl::Begin)
        );
        assert_eq!(
            transaction_control("START TRANSACTION"),
            Some(TxControl::Begin)
        );
        assert_eq!(transaction_control("COMMIT"), Some(TxControl::Commit));
        assert_eq!(transaction_control("END"), Some(TxControl::Commit));
        assert_eq!(transaction_control("ROLLBACK"), Some(TxControl::Rollback));
        assert_eq!(transaction_control("ABORT"), Some(TxControl::Rollback));
        assert_eq!(transaction_control("SELECT 1"), None);
    }

    #[test]
    fn savepoint_statements_are_forwarded_not_intercepted() {
        // These work inside a Data API transaction as written.
        assert_eq!(transaction_control("SAVEPOINT a"), None);
        assert_eq!(transaction_control("RELEASE SAVEPOINT a"), None);
        assert_eq!(transaction_control("ROLLBACK TO SAVEPOINT a"), None);
        assert_eq!(transaction_control("ROLLBACK TO a"), None);
    }

    #[test]
    fn maps_result_type_oids() {
        assert_eq!(types_from_oids(&[23, 25]), vec![Type::INT4, Type::TEXT]);
        // An OID the proxy does not know becomes text rather than a guess.
        assert_eq!(types_from_oids(&[999_999]), vec![Type::TEXT]);
        // And so does one it knows but cannot deliver: the Data API will not
        // return an interval at all, so describing it as one would be a lie.
        assert_eq!(
            types_from_oids(&[Type::INTERVAL.oid() as i64]),
            vec![Type::TEXT]
        );
    }
}
