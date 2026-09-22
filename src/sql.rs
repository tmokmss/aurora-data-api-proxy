//! Token-level scanning of SQL text.
//!
//! The proxy has to rewrite PostgreSQL's positional `$1` placeholders into the
//! named `:p1` placeholders the Data API expects, split multi-statement Simple
//! Query strings (the Data API rejects more than one statement per call), and
//! recognise a handful of constructs the Data API's own parameter scanner gets
//! wrong. All of that needs to know where string literals, quoted identifiers
//! and comments begin and end -- but none of it needs a real grammar, so this
//! is a lexer and nothing more.

/// A lexical region of a SQL string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// Ordinary SQL, outside any literal or comment.
    Code,
    /// A `'...'` string literal, including the quotes.
    SingleQuoted,
    /// A `"..."` quoted identifier, including the quotes.
    DoubleQuoted,
    /// A `$tag$...$tag$` string literal, including both delimiters.
    DollarQuoted,
    /// A `-- ...` comment, up to but not including the newline.
    LineComment,
    /// A `/* ... */` comment, which may nest.
    BlockComment,
}

/// A `Segment` together with its byte range in the source string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub segment: Segment,
    pub start: usize,
    pub end: usize,
}

impl Span {
    /// The text of this span.
    pub fn text<'a>(&self, sql: &'a str) -> &'a str {
        &sql[self.start..self.end]
    }

    /// Whether this span holds SQL that the server will interpret, as opposed
    /// to literal text or a comment.
    pub fn is_code(&self) -> bool {
        self.segment == Segment::Code
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

/// If a dollar-quote delimiter starts at `i`, return its length (2 for `$$`,
/// 5 for `$tag$`).
///
/// A `$` that introduces a placeholder (`$1`) or that is merely part of an
/// identifier is not a delimiter, so this returns `None` for those.
fn dollar_delimiter_len(bytes: &[u8], i: usize) -> Option<usize> {
    debug_assert_eq!(bytes[i], b'$');
    let mut j = i + 1;
    while j < bytes.len() && bytes[j] != b'$' {
        // A tag is an identifier: it may not start with a digit.
        let first = j == i + 1;
        if first && !is_ident_start(bytes[j]) {
            return None;
        }
        if !first && !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] >= 0x80) {
            return None;
        }
        j += 1;
    }
    if j < bytes.len() && bytes[j] == b'$' {
        Some(j - i + 1)
    } else {
        None
    }
}

/// Split `sql` into lexical spans.
///
/// The spans tile the whole input: concatenating them reproduces it exactly.
/// An unterminated literal or comment runs to the end of the input rather than
/// producing an error; the server is the authority on whether the SQL is
/// valid, and reporting that is its job, not ours.
pub fn scan(sql: &str) -> Vec<Span> {
    let bytes = sql.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    let mut code_start = 0;

    macro_rules! flush_code {
        ($at:expr) => {
            if $at > code_start {
                spans.push(Span {
                    segment: Segment::Code,
                    start: code_start,
                    end: $at,
                });
            }
        };
    }

    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                flush_code!(i);
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' && i + 1 < bytes.len() {
                        // Backslash escapes only apply in E'' strings, but
                        // treating them as escapes everywhere is harmless: a
                        // standard-conforming string cannot end on the byte
                        // after a backslash either way.
                        i += 2;
                    } else if bytes[i] == b'\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            i += 2; // '' is an escaped quote
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                spans.push(Span {
                    segment: Segment::SingleQuoted,
                    start,
                    end: i,
                });
                code_start = i;
            }
            b'"' => {
                flush_code!(i);
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        if bytes.get(i + 1) == Some(&b'"') {
                            i += 2; // "" is an escaped quote
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                spans.push(Span {
                    segment: Segment::DoubleQuoted,
                    start,
                    end: i,
                });
                code_start = i;
            }
            b'$' => {
                if let Some(delim_len) = dollar_delimiter_len(bytes, i) {
                    flush_code!(i);
                    let start = i;
                    let delim = &bytes[i..i + delim_len];
                    i += delim_len;
                    while i < bytes.len() {
                        if bytes[i] == b'$' && bytes[i..].starts_with(delim) {
                            i += delim_len;
                            break;
                        }
                        i += 1;
                    }
                    spans.push(Span {
                        segment: Segment::DollarQuoted,
                        start,
                        end: i,
                    });
                    code_start = i;
                } else {
                    i += 1;
                }
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                flush_code!(i);
                let start = i;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                spans.push(Span {
                    segment: Segment::LineComment,
                    start,
                    end: i,
                });
                code_start = i;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                flush_code!(i);
                let start = i;
                i += 2;
                let mut depth = 1usize;
                while i < bytes.len() && depth > 0 {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                }
                spans.push(Span {
                    segment: Segment::BlockComment,
                    start,
                    end: i,
                });
                code_start = i;
            }
            _ => i += 1,
        }
    }
    flush_code!(bytes.len());
    spans
}

/// The result of rewriting `$n` placeholders into `:pn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewritten {
    /// The SQL with `$n` replaced by `:pn`.
    pub sql: String,
    /// The highest placeholder index seen, i.e. the number of parameters the
    /// statement expects.
    pub param_count: usize,
}

/// The Data API placeholder name for parameter `n` (1-based).
pub fn param_name(n: usize) -> String {
    format!("p{n}")
}

/// Rewrite PostgreSQL positional placeholders into Data API named ones.
///
/// Only `$n` occurrences in code are rewritten: those inside string literals,
/// quoted identifiers, dollar-quoted bodies and comments are left alone.
pub fn rewrite_placeholders(sql: &str) -> Rewritten {
    rewrite_placeholders_with_casts(sql, &[])
}

/// Rewrite `$n` into `:pn`, wrapping each placeholder in an explicit cast to
/// the type named in `cast_types[n - 1]`, where one is known.
///
/// The cast is what makes bound parameters work at all. The Data API binds a
/// JSON string as PostgreSQL `text` and nothing else, so `WHERE id = :p1`
/// against an integer column fails outright with
/// `operator does not exist: integer = text`. Naming the type turns that into
/// an ordinary explicit cast, which PostgreSQL performs from text exactly as it
/// would when parsing a literal.
///
/// Casting also fixes two subtler problems for free: a NULL parameter arrives
/// typed rather than as an untyped `NULL`, and a `timestamptz` keeps the zone
/// offset the client sent, because PostgreSQL -- not the Data API -- is the one
/// interpreting the text.
pub fn rewrite_placeholders_with_casts(sql: &str, cast_types: &[Option<String>]) -> Rewritten {
    map_placeholders(sql, |n| {
        match cast_types.get(n - 1).and_then(Option::as_deref) {
            Some(ty) => format!("CAST(:{} AS {ty})", param_name(n)),
            None => format!(":{}", param_name(n)),
        }
    })
}

/// Replace every `$n` with a typed NULL.
///
/// This is how the proxy asks PostgreSQL for a statement's result columns
/// without having any parameter values: the statement becomes a plain query
/// that can be wrapped in `SELECT * FROM (...) WHERE false`, which the planner
/// reduces to a one-time false filter and never runs.
pub fn substitute_null_params(sql: &str, types: &[Option<String>]) -> String {
    map_placeholders(sql, |n| match types.get(n - 1).and_then(Option::as_deref) {
        Some(ty) => format!("NULL::{ty}"),
        None => "NULL".to_string(),
    })
    .sql
}

/// Rewrite each `$n` in code with whatever `replacement` returns for it.
fn map_placeholders<F: Fn(usize) -> String>(sql: &str, replacement: F) -> Rewritten {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut param_count = 0usize;

    for span in scan(sql) {
        if !span.is_code() {
            out.push_str(span.text(sql));
            continue;
        }
        let mut i = span.start;
        while i < span.end {
            if bytes[i] == b'$' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                let mut j = i + 1;
                while j < span.end && bytes[j].is_ascii_digit() {
                    j += 1;
                }
                // A placeholder index that does not fit in a usize is not one
                // we can bind; leave it for the server to reject.
                match sql[i + 1..j].parse::<usize>() {
                    Ok(n) if n >= 1 => {
                        param_count = param_count.max(n);
                        out.push_str(&replacement(n));
                    }
                    _ => out.push_str(&sql[i..j]),
                }
                i = j;
            } else {
                let ch_len = utf8_len(bytes[i]);
                out.push_str(&sql[i..i + ch_len]);
                i += ch_len;
            }
        }
    }

    Rewritten {
        sql: out,
        param_count,
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Split a Simple Query string into individual statements.
///
/// The Data API rejects more than one statement per call
/// (`Multistatements aren't supported`), so the proxy runs them one at a time.
/// Semicolons inside literals and comments do not split, and fragments holding
/// only whitespace or comments are dropped.
pub fn split_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;

    for span in scan(sql) {
        if !span.is_code() {
            continue;
        }
        let mut i = span.start;
        while i < span.end {
            if bytes[i] == b';' {
                let stmt = &sql[start..i];
                if !is_blank_statement(stmt) {
                    out.push(stmt.to_string());
                }
                start = i + 1;
            }
            i += 1;
        }
    }
    let tail = &sql[start..];
    if !is_blank_statement(tail) {
        out.push(tail.to_string());
    }
    out
}

/// Whether a statement carries no SQL at all -- only whitespace and comments.
pub fn is_blank_statement(sql: &str) -> bool {
    scan(sql)
        .iter()
        .filter(|s| !matches!(s.segment, Segment::LineComment | Segment::BlockComment))
        .all(|s| s.text(sql).trim().is_empty())
}

/// The words that open a statement, uppercased, ignoring leading comments.
///
/// Returns at most `max` words, which is all any caller here needs.
pub fn leading_words(sql: &str, max: usize) -> Vec<String> {
    let mut words = Vec::new();
    let bytes = sql.as_bytes();
    for span in scan(sql) {
        match span.segment {
            Segment::LineComment | Segment::BlockComment => continue,
            Segment::Code => {}
            // A statement opening with a literal or quoted identifier has no
            // leading keyword to report.
            _ => return words,
        }
        let mut i = span.start;
        while i < span.end && words.len() < max {
            if is_ident_start(bytes[i]) {
                let start = i;
                while i < span.end && is_ident_char(bytes[i]) {
                    i += 1;
                }
                words.push(sql[start..i].to_ascii_uppercase());
            } else if bytes[i].is_ascii_whitespace() {
                i += 1;
            } else {
                // Punctuation such as the `(` of `(SELECT ...)`: not a keyword,
                // and nothing after it is a leading keyword either.
                return words;
            }
        }
        if words.len() >= max {
            return words;
        }
    }
    words
}

/// Whether the statement could change what another statement's shape is.
///
/// The proxy caches the parameter and result types it works out for a SQL
/// text, and describes statements to clients from that cache. A column added,
/// dropped or retyped underneath it would leave a client decoding rows against
/// a description that no longer matches them, so anything that might do that
/// empties the cache.
///
/// Only the leading keyword is examined, which is what distinguishes DDL from
/// a query that merely mentions `create` in a string or a column name. It
/// cannot see DDL run through `EXECUTE`, inside a function, or by anything
/// that is not this proxy -- see the note in COMPATIBILITY.md.
pub fn changes_schema(sql: &str) -> bool {
    const DDL_KEYWORDS: [&str; 5] = ["CREATE", "ALTER", "DROP", "REINDEX", "REFRESH"];
    match leading_words(sql, 1).first() {
        Some(word) => DDL_KEYWORDS.contains(&word.as_str()),
        None => false,
    }
}

/// Whether the statement modifies data anywhere in its text.
///
/// Used to decide whether a statement can safely be probed with
/// `CREATE TEMP TABLE ... AS EXECUTE ... WITH NO DATA`. That probe does not
/// execute a plain `SELECT`, but it *does* execute a data-modifying CTE
/// (verified against Aurora PostgreSQL 17.9), so any statement mentioning one
/// of these keywords is off limits.
pub fn modifies_data(sql: &str) -> bool {
    const WRITE_KEYWORDS: [&str; 4] = ["INSERT", "UPDATE", "DELETE", "MERGE"];
    let bytes = sql.as_bytes();
    for span in scan(sql) {
        if !span.is_code() {
            continue;
        }
        let mut i = span.start;
        while i < span.end {
            if is_ident_start(bytes[i]) {
                let start = i;
                while i < span.end && is_ident_char(bytes[i]) {
                    i += 1;
                }
                if WRITE_KEYWORDS.contains(&sql[start..i].to_ascii_uppercase().as_str()) {
                    return true;
                }
            } else {
                i += 1;
            }
        }
    }
    false
}

/// A construct that the Data API's own parameter scanner mis-parses.
///
/// The Data API looks for `:name` placeholders without respecting
/// dollar-quoted strings or array-slice syntax. Both were verified against the
/// live service: `SELECT $$a :b c$$` comes back as `a $1 c` -- silently
/// corrupted -- and `(ARRAY[1,2,3])[1:2]` fails with a syntax error on `$1`.
/// The proxy refuses such statements rather than passing them through, because
/// the first case returns a wrong answer with no error at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hazard {
    /// A `:name` inside a dollar-quoted body; the Data API would rewrite it.
    ColonInDollarQuote { text: String },
    /// An array slice `[m:n]`; the Data API reads `:n` as a placeholder.
    ArraySlice,
}

impl Hazard {
    /// A message explaining the refusal, in the form the client will see.
    pub fn message(&self) -> String {
        match self {
            Hazard::ColonInDollarQuote { text } => format!(
                "the Aurora Data API rewrites `{text}` inside a dollar-quoted string as if it were \
                 a bind parameter, which would silently corrupt the string; use a single-quoted \
                 literal instead"
            ),
            Hazard::ArraySlice => "the Aurora Data API reads the `:` of an array slice such as \
                 `a[1:2]` as a bind parameter; select the whole array and slice it in the client, \
                 or use a function such as trim_array instead"
                .to_string(),
        }
    }
}

/// Find a construct the Data API would mis-parse, if the statement has one.
///
/// `sql` is the statement *after* placeholder rewriting, so the `:pN` names the
/// proxy itself introduced are in code spans and are expected there.
pub fn find_hazard(sql: &str) -> Option<Hazard> {
    let bytes = sql.as_bytes();
    for span in scan(sql) {
        match span.segment {
            Segment::DollarQuoted => {
                let body = span.text(sql).as_bytes();
                let mut i = 0;
                while i < body.len() {
                    if body[i] == b':' && body.get(i + 1).is_some_and(|&b| is_ident_start(b)) {
                        let start = i;
                        i += 1;
                        while i < body.len() && is_ident_char(body[i]) {
                            i += 1;
                        }
                        return Some(Hazard::ColonInDollarQuote {
                            text: String::from_utf8_lossy(&body[start..i]).into_owned(),
                        });
                    }
                    i += 1;
                }
            }
            Segment::Code => {
                // An array slice is `[` ... `:` ... `]` with no intervening
                // bracket. A `::` cast is not a slice boundary.
                let mut i = span.start;
                while i < span.end {
                    if bytes[i] == b'[' {
                        let mut j = i + 1;
                        while j < span.end && bytes[j] != b']' && bytes[j] != b'[' {
                            if bytes[j] == b':' {
                                if bytes.get(j + 1) == Some(&b':') {
                                    j += 2;
                                    continue;
                                }
                                return Some(Hazard::ArraySlice);
                            }
                            j += 1;
                        }
                    }
                    i += 1;
                }
            }
            _ => {}
        }
    }
    None
}

/// One item in a `RETURNING` list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturningItem {
    /// An output column whose name we know.
    Named(String),
    /// `*` or `tbl.*`, standing for every column of the target table.
    Star,
    /// An expression with no alias; PostgreSQL names these `?column?`.
    Unnamed,
}

/// The tokens of a code span, as (text, is_quoted_identifier) pairs.
fn tokens(sql: &str) -> Vec<(String, bool)> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    for span in scan(sql) {
        match span.segment {
            Segment::DoubleQuoted => {
                let raw = span.text(sql);
                let inner = raw
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or(raw);
                out.push((inner.replace("\"\"", "\""), true));
            }
            Segment::Code => {
                let mut i = span.start;
                while i < span.end {
                    if is_ident_start(bytes[i]) || bytes[i].is_ascii_digit() {
                        let start = i;
                        while i < span.end && is_ident_char(bytes[i]) {
                            i += 1;
                        }
                        out.push((sql[start..i].to_string(), false));
                    } else if bytes[i].is_ascii_whitespace() {
                        i += 1;
                    } else {
                        out.push((sql[i..i + 1].to_string(), false));
                        i += 1;
                    }
                }
            }
            _ => out.push((span.text(sql).to_string(), false)),
        }
    }
    out
}

/// Find the byte offset just past a top-level `RETURNING` keyword.
fn returning_offset(sql: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut depth = 0i32;
    for span in scan(sql) {
        if !span.is_code() {
            continue;
        }
        let mut i = span.start;
        while i < span.end {
            if is_ident_start(bytes[i]) {
                let start = i;
                while i < span.end && is_ident_char(bytes[i]) {
                    i += 1;
                }
                if depth == 0 && sql[start..i].eq_ignore_ascii_case("returning") {
                    return Some(i);
                }
            } else {
                match bytes[i] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
        }
    }
    None
}

/// Split a select list at its top-level commas.
fn split_select_list(list: &str) -> Vec<String> {
    let bytes = list.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for span in scan(list) {
        if !span.is_code() {
            continue;
        }
        for i in span.start..span.end {
            match bytes[i] {
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                b',' if depth == 0 => {
                    out.push(list[start..i].to_string());
                    start = i + 1;
                }
                _ => {}
            }
        }
    }
    out.push(list[start..].to_string());
    out
}

/// Parse the `RETURNING` list of a data-modifying statement.
///
/// Returns `None` when there is no top-level `RETURNING`, which means the
/// statement produces no rows.
///
/// This exists because there is no way to ask Aurora for the output columns of
/// a `RETURNING` clause without running the statement. `pg_prepared_statements`
/// gives their types but not their names, and the trick that recovers names for
/// a `SELECT` -- `CREATE TABLE ... AS EXECUTE ... WITH NO DATA` -- refuses a
/// statement that is not a `SELECT`, and *does* perform the write when the
/// statement is wrapped in a data-modifying CTE. Reading the names off the
/// clause is what is left, and it is exact for every form real clients send.
pub fn returning_items(sql: &str) -> Option<Vec<ReturningItem>> {
    let offset = returning_offset(sql)?;
    let items = split_select_list(&sql[offset..])
        .iter()
        .map(|item| classify_returning_item(item))
        .collect::<Vec<_>>();
    Some(items)
}

fn classify_returning_item(item: &str) -> ReturningItem {
    let toks = tokens(item);
    if toks.is_empty() {
        return ReturningItem::Unnamed;
    }
    // `*` on its own, or `tbl.*`.
    if toks.last().map(|(t, q)| t == "*" && !q).unwrap_or(false) {
        return ReturningItem::Star;
    }
    // An explicit alias: `... AS name`.
    if toks.len() >= 2 {
        let (last, last_quoted) = &toks[toks.len() - 1];
        let (prev, prev_quoted) = &toks[toks.len() - 2];
        if !prev_quoted && prev.eq_ignore_ascii_case("as") && is_name_token(last, *last_quoted) {
            return ReturningItem::Named(normalise_name(last, *last_quoted));
        }
    }
    // A bare column reference: `col`, `tbl.col`, `"My Tbl"."My Col"`.
    if is_column_reference(&toks) {
        let (last, quoted) = toks.last().expect("checked non-empty");
        return ReturningItem::Named(normalise_name(last, *quoted));
    }
    ReturningItem::Unnamed
}

fn is_name_token(t: &str, quoted: bool) -> bool {
    quoted || t.as_bytes().first().is_some_and(|b| is_ident_start(*b))
}

/// Whether the tokens form `a`, `a.b` or `a.b.c` and nothing else.
fn is_column_reference(toks: &[(String, bool)]) -> bool {
    if toks.len().is_multiple_of(2) {
        return false;
    }
    for (i, (t, quoted)) in toks.iter().enumerate() {
        if i % 2 == 0 {
            if !is_name_token(t, *quoted) {
                return false;
            }
        } else if *quoted || t != "." {
            return false;
        }
    }
    true
}

fn normalise_name(t: &str, quoted: bool) -> String {
    if quoted {
        // A quoted identifier keeps its case exactly.
        t.to_string()
    } else {
        // An unquoted one is folded to lower case, as PostgreSQL does.
        t.to_ascii_lowercase()
    }
}

/// The table a data-modifying statement writes to, as written in the SQL.
///
/// Used to expand `RETURNING *` into real column names.
pub fn dml_target_table(sql: &str) -> Option<String> {
    let toks = tokens(sql);
    let mut idx = 0usize;
    // Skip leading comments, which `tokens` keeps.
    while idx < toks.len() && (toks[idx].0.starts_with("--") || toks[idx].0.starts_with("/*")) {
        idx += 1;
    }
    let first = toks.get(idx)?;
    if first.1 {
        return None;
    }
    let skip = match first.0.to_ascii_uppercase().as_str() {
        "INSERT" | "MERGE" => {
            let second = toks.get(idx + 1)?;
            if second.1 || !second.0.eq_ignore_ascii_case("into") {
                return None;
            }
            2
        }
        "DELETE" => {
            let second = toks.get(idx + 1)?;
            if second.1 || !second.0.eq_ignore_ascii_case("from") {
                return None;
            }
            2
        }
        "UPDATE" => 1,
        _ => return None,
    };

    // The table name may be schema-qualified, and either part may be quoted.
    let mut name = String::new();
    let mut i = idx + skip;
    loop {
        let (t, quoted) = toks.get(i)?;
        if !is_name_token(t, *quoted) {
            return None;
        }
        name.push_str(&quote_if_needed(t, *quoted));
        match toks.get(i + 1) {
            Some((dot, false)) if dot == "." => {
                name.push('.');
                i += 2;
            }
            _ => break,
        }
    }
    Some(name)
}

/// Re-quote an identifier so it can be pasted back into SQL unchanged.
fn quote_if_needed(t: &str, was_quoted: bool) -> String {
    if was_quoted {
        format!("\"{}\"", t.replace('"', "\"\""))
    } else {
        t.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rw(sql: &str) -> String {
        rewrite_placeholders(sql).sql
    }

    #[test]
    fn rewrites_plain_placeholders() {
        let r = rewrite_placeholders("SELECT * FROM t WHERE a = $1 AND b = $2");
        assert_eq!(r.sql, "SELECT * FROM t WHERE a = :p1 AND b = :p2");
        assert_eq!(r.param_count, 2);
    }

    #[test]
    fn placeholder_count_is_the_highest_index() {
        assert_eq!(rewrite_placeholders("SELECT $2, $1").param_count, 2);
        assert_eq!(rewrite_placeholders("SELECT $10").param_count, 10);
        assert_eq!(rewrite_placeholders("SELECT 1").param_count, 0);
    }

    #[test]
    fn leaves_placeholders_in_single_quotes_alone() {
        assert_eq!(rw("SELECT 'cost: $1' , $1"), "SELECT 'cost: $1' , :p1");
    }

    #[test]
    fn leaves_placeholders_in_doubled_quotes_alone() {
        assert_eq!(rw("SELECT 'it''s $1', $1"), "SELECT 'it''s $1', :p1");
    }

    #[test]
    fn leaves_placeholders_in_quoted_identifiers_alone() {
        assert_eq!(rw(r#"SELECT "col $1", $1"#), r#"SELECT "col $1", :p1"#);
    }

    #[test]
    fn leaves_placeholders_in_dollar_quotes_alone() {
        assert_eq!(rw("SELECT $$a $1 b$$, $1"), "SELECT $$a $1 b$$, :p1");
        assert_eq!(
            rw("SELECT $fn$body $1$fn$, $2"),
            "SELECT $fn$body $1$fn$, :p2"
        );
    }

    #[test]
    fn leaves_placeholders_in_comments_alone() {
        assert_eq!(
            rw("SELECT $1 -- not $2\n, $3"),
            "SELECT :p1 -- not $2\n, :p3"
        );
        assert_eq!(rw("SELECT /* $9 */ $1"), "SELECT /* $9 */ :p1");
    }

    #[test]
    fn handles_nested_block_comments() {
        assert_eq!(
            rw("SELECT /* a /* $9 */ b */ $1"),
            "SELECT /* a /* $9 */ b */ :p1"
        );
    }

    #[test]
    fn does_not_mistake_casts_for_placeholders() {
        assert_eq!(rw("SELECT $1::int"), "SELECT :p1::int");
    }

    #[test]
    fn wraps_placeholders_in_casts_when_the_type_is_known() {
        let types = vec![Some("integer".to_string()), Some("text".to_string())];
        let r = rewrite_placeholders_with_casts("SELECT * FROM t WHERE a = $1 AND b = $2", &types);
        assert_eq!(
            r.sql,
            "SELECT * FROM t WHERE a = CAST(:p1 AS integer) AND b = CAST(:p2 AS text)"
        );
        assert_eq!(r.param_count, 2);
    }

    #[test]
    fn leaves_a_placeholder_bare_when_its_type_is_unknown() {
        let types = vec![None, Some("text".to_string())];
        let r = rewrite_placeholders_with_casts("SELECT $1, $2", &types);
        assert_eq!(r.sql, "SELECT :p1, CAST(:p2 AS text)");
    }

    #[test]
    fn casts_every_occurrence_of_a_repeated_placeholder() {
        let types = vec![Some("integer".to_string())];
        let r = rewrite_placeholders_with_casts("SELECT $1 WHERE $1 > 0", &types);
        assert_eq!(
            r.sql,
            "SELECT CAST(:p1 AS integer) WHERE CAST(:p1 AS integer) > 0"
        );
    }

    #[test]
    fn substitutes_typed_nulls_for_placeholders() {
        let types = vec![Some("integer".to_string()), Some("text".to_string())];
        assert_eq!(
            substitute_null_params("SELECT * FROM t WHERE a = $1 AND b = $2", &types),
            "SELECT * FROM t WHERE a = NULL::integer AND b = NULL::text"
        );
    }

    #[test]
    fn substitutes_a_bare_null_when_the_type_is_unknown() {
        assert_eq!(substitute_null_params("SELECT $1", &[None]), "SELECT NULL");
    }

    #[test]
    fn null_substitution_respects_literals() {
        assert_eq!(
            substitute_null_params("SELECT '$1', $1", &[Some("int4".to_string())]),
            "SELECT '$1', NULL::int4"
        );
    }

    #[test]
    fn casting_still_respects_literals() {
        let types = vec![Some("integer".to_string())];
        let r = rewrite_placeholders_with_casts("SELECT '$1', $1", &types);
        assert_eq!(r.sql, "SELECT '$1', CAST(:p1 AS integer)");
    }

    #[test]
    fn dollar_in_identifier_is_not_a_quote() {
        // `a$b` is a legal identifier; it must not open a dollar-quoted string.
        assert_eq!(rw("SELECT a$b, $1"), "SELECT a$b, :p1");
    }

    #[test]
    fn unterminated_literal_does_not_panic() {
        assert_eq!(rw("SELECT 'abc"), "SELECT 'abc");
        assert_eq!(rw("SELECT $$abc"), "SELECT $$abc");
        assert_eq!(rw("SELECT /* abc"), "SELECT /* abc");
        assert_eq!(rw(r#"SELECT "abc"#), r#"SELECT "abc"#);
    }

    #[test]
    fn multibyte_text_survives_rewriting() {
        assert_eq!(rw("SELECT '日本語', $1"), "SELECT '日本語', :p1");
        assert_eq!(
            rw("SELECT 日本 FROM t WHERE x = $1"),
            "SELECT 日本 FROM t WHERE x = :p1"
        );
    }

    #[test]
    fn splits_statements() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2"),
            vec!["SELECT 1", " SELECT 2"]
        );
    }

    #[test]
    fn does_not_split_on_semicolons_in_literals() {
        assert_eq!(split_statements("SELECT 'a;b'"), vec!["SELECT 'a;b'"]);
        assert_eq!(split_statements("SELECT $$a;b$$"), vec!["SELECT $$a;b$$"]);
        assert_eq!(
            split_statements("SELECT 1 -- a;b\n"),
            vec!["SELECT 1 -- a;b\n"]
        );
    }

    #[test]
    fn drops_empty_trailing_statements() {
        assert_eq!(split_statements("SELECT 1;"), vec!["SELECT 1"]);
        assert_eq!(split_statements("SELECT 1;;  "), vec!["SELECT 1"]);
        assert!(split_statements("  ;  ").is_empty());
        assert!(split_statements("-- just a comment").is_empty());
    }

    #[test]
    fn recognises_blank_statements() {
        assert!(is_blank_statement(""));
        assert!(is_blank_statement("   \n\t "));
        assert!(is_blank_statement("-- hi"));
        assert!(is_blank_statement("/* hi */"));
        assert!(!is_blank_statement("SELECT 1"));
        assert!(!is_blank_statement("/* hi */ SELECT 1"));
    }

    #[test]
    fn reads_leading_keywords() {
        assert_eq!(leading_words("select * from t", 1), vec!["SELECT"]);
        assert_eq!(
            leading_words("  /* c */ START   TRANSACTION", 2),
            vec!["START", "TRANSACTION"]
        );
        assert_eq!(leading_words("-- c\nBEGIN", 1), vec!["BEGIN"]);
        assert_eq!(leading_words("", 1), Vec::<String>::new());
        assert_eq!(leading_words("(SELECT 1)", 1), Vec::<String>::new());
    }

    #[test]
    fn detects_data_modifying_statements() {
        assert!(modifies_data("INSERT INTO t VALUES (1)"));
        assert!(modifies_data(
            "WITH x AS (INSERT INTO t VALUES (1) RETURNING id) SELECT * FROM x"
        ));
        assert!(modifies_data("select * from t for update"));
        assert!(!modifies_data("SELECT * FROM t"));
        // A write keyword inside a literal or comment is not a write.
        assert!(!modifies_data("SELECT 'INSERT INTO t'"));
        assert!(!modifies_data("SELECT * FROM t -- INSERT"));
        // ...and a column called `updated_at` must not trip it either.
        assert!(!modifies_data("SELECT updated_at FROM t"));
    }

    #[test]
    fn detects_statements_that_reshape_the_schema() {
        assert!(changes_schema("CREATE TABLE t (a int)"));
        assert!(changes_schema("create table t (a int)"));
        assert!(changes_schema("ALTER TABLE t ADD COLUMN b text"));
        assert!(changes_schema("DROP VIEW v"));
        assert!(changes_schema("  \n  CREATE INDEX i ON t (a)"));
        assert!(changes_schema("-- a comment first\nDROP TABLE t"));
        assert!(changes_schema("CREATE OR REPLACE VIEW v AS SELECT 1"));
        assert!(changes_schema("REFRESH MATERIALIZED VIEW m"));
    }

    #[test]
    fn leaves_the_cache_alone_for_statements_that_only_mention_ddl() {
        assert!(!changes_schema("SELECT * FROM t"));
        assert!(!changes_schema("INSERT INTO t VALUES (1)"));
        // The keyword is only a keyword when it leads.
        assert!(!changes_schema("SELECT 'CREATE TABLE t'"));
        assert!(!changes_schema("SELECT created_at FROM t"));
        assert!(!changes_schema("SELECT * FROM t WHERE x = 'DROP'"));
        assert!(!changes_schema("UPDATE t SET altered = true"));
        assert!(!changes_schema(""));
        // The probe's own statements must not empty the cache it fills.
        assert!(!changes_schema("PREPARE dapi_ps_1 AS SELECT 1"));
        assert!(!changes_schema("SAVEPOINT dapi_probe_1"));
    }

    #[test]
    fn flags_colon_names_inside_dollar_quotes() {
        assert_eq!(
            find_hazard("SELECT $$a :b c$$"),
            Some(Hazard::ColonInDollarQuote {
                text: ":b".to_string()
            })
        );
        assert!(find_hazard("SELECT $$a b c$$").is_none());
        // A colon in a single-quoted string is fine: the Data API respects those.
        assert!(find_hazard("SELECT 'a :b c'").is_none());
        assert!(find_hazard(r#"SELECT '{"a":1}'::jsonb"#).is_none());
    }

    #[test]
    fn flags_array_slices() {
        assert_eq!(
            find_hazard("SELECT a[1:2] FROM t"),
            Some(Hazard::ArraySlice)
        );
        assert_eq!(
            find_hazard("SELECT (ARRAY[1,2,3])[1:2]"),
            Some(Hazard::ArraySlice)
        );
        // Subscripts and casts are not slices.
        assert!(find_hazard("SELECT a[1] FROM t").is_none());
        assert!(find_hazard("SELECT a[1]::text FROM t").is_none());
        assert!(find_hazard("SELECT :p1::int").is_none());
        assert!(find_hazard("SELECT ARRAY[1,2,3]").is_none());
    }

    fn named(names: &[&str]) -> Vec<ReturningItem> {
        names
            .iter()
            .map(|n| ReturningItem::Named((*n).to_string()))
            .collect()
    }

    #[test]
    fn reads_a_simple_returning_list() {
        assert_eq!(
            returning_items("INSERT INTO t(a) VALUES ($1) RETURNING id, name"),
            Some(named(&["id", "name"]))
        );
    }

    #[test]
    fn reads_aliases_in_a_returning_list() {
        assert_eq!(
            returning_items("INSERT INTO t(a) VALUES ($1) RETURNING amount*2 AS doubled"),
            Some(named(&["doubled"]))
        );
        assert_eq!(
            returning_items(r#"UPDATE t SET a = 1 RETURNING a AS "My Col""#),
            Some(named(&["My Col"]))
        );
    }

    #[test]
    fn folds_unquoted_returning_names_to_lower_case() {
        // PostgreSQL does, so the client will look the column up in lower case.
        assert_eq!(
            returning_items("INSERT INTO t(a) VALUES (1) RETURNING ID, Name"),
            Some(named(&["id", "name"]))
        );
    }

    #[test]
    fn reads_qualified_returning_names() {
        assert_eq!(
            returning_items("UPDATE t SET a = 1 RETURNING t.id, t.name"),
            Some(named(&["id", "name"]))
        );
    }

    #[test]
    fn marks_unaliased_expressions_as_unnamed() {
        assert_eq!(
            returning_items("INSERT INTO t(a) VALUES (1) RETURNING id, amount * 2"),
            Some(vec![
                ReturningItem::Named("id".into()),
                ReturningItem::Unnamed
            ])
        );
    }

    #[test]
    fn recognises_returning_star() {
        assert_eq!(
            returning_items("DELETE FROM t WHERE a = 1 RETURNING *"),
            Some(vec![ReturningItem::Star])
        );
        assert_eq!(
            returning_items("DELETE FROM t WHERE a = 1 RETURNING t.*"),
            Some(vec![ReturningItem::Star])
        );
    }

    #[test]
    fn does_not_split_a_returning_list_inside_parentheses() {
        assert_eq!(
            returning_items("INSERT INTO t(a) VALUES (1) RETURNING greatest(a, b) AS g, c"),
            Some(vec![
                ReturningItem::Named("g".into()),
                ReturningItem::Named("c".into())
            ])
        );
    }

    #[test]
    fn finds_no_returning_when_there_is_none() {
        assert_eq!(returning_items("INSERT INTO t(a) VALUES (1)"), None);
        // A `returning` inside a literal is not the keyword.
        assert_eq!(
            returning_items("INSERT INTO t(a) VALUES ('returning')"),
            None
        );
    }

    #[test]
    fn ignores_a_returning_nested_in_a_subquery() {
        // The top-level statement here is a SELECT; its columns come from the
        // outer select list, not from the CTE's RETURNING.
        assert_eq!(
            returning_items("WITH x AS (INSERT INTO t(a) VALUES (1) RETURNING id) SELECT * FROM x"),
            None
        );
    }

    #[test]
    fn finds_the_target_table_of_a_dml_statement() {
        assert_eq!(
            dml_target_table("INSERT INTO t(a) VALUES (1) RETURNING *").as_deref(),
            Some("t")
        );
        assert_eq!(
            dml_target_table("insert into public.t (a) values (1)").as_deref(),
            Some("public.t")
        );
        assert_eq!(
            dml_target_table("UPDATE t SET a = 1 RETURNING *").as_deref(),
            Some("t")
        );
        assert_eq!(
            dml_target_table("DELETE FROM public.t WHERE a = 1").as_deref(),
            Some("public.t")
        );
        assert_eq!(
            dml_target_table(r#"INSERT INTO "My Table"(a) VALUES (1)"#).as_deref(),
            Some(r#""My Table""#)
        );
        assert_eq!(dml_target_table("SELECT * FROM t"), None);
    }
}
