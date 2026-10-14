//! Alias-aware completion for SQL and Redis.
use crate::db::DbKind;
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct SchemaCache {
    pub tables: Vec<String>,
    pub columns: Vec<(String, String)>,
}

pub(super) const SQL_WORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "AND",
    "OR",
    "NOT",
    "NULL",
    "IS",
    "IN",
    "LIKE",
    "ILIKE",
    "BETWEEN",
    "EXISTS",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "CREATE",
    "TABLE",
    "VIEW",
    "INDEX",
    "DROP",
    "ALTER",
    "ADD",
    "COLUMN",
    "PRIMARY",
    "KEY",
    "FOREIGN",
    "REFERENCES",
    "UNIQUE",
    "DEFAULT",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "CROSS",
    "ON",
    "USING",
    "GROUP",
    "BY",
    "ORDER",
    "ASC",
    "DESC",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "UNION",
    "ALL",
    "DISTINCT",
    "AS",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "TRANSACTION",
    "WITH",
    "RETURNING",
    "COUNT",
    "SUM",
    "AVG",
    "MIN",
    "MAX",
    "COALESCE",
    "CAST",
    "EXPLAIN",
    "ANALYZE",
    "TRUNCATE",
    "SHOW",
    "DESCRIBE",
    "CURRENT_TIMESTAMP",
    "NOW",
    "TRUE",
    "FALSE",
];

const REDIS_WORDS: &[&str] = &[
    "GET",
    "SET",
    "DEL",
    "EXISTS",
    "EXPIRE",
    "TTL",
    "PERSIST",
    "TYPE",
    "KEYS",
    "SCAN",
    "RENAME",
    "INCR",
    "INCRBY",
    "DECR",
    "APPEND",
    "MGET",
    "MSET",
    "HGET",
    "HSET",
    "HDEL",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HINCRBY",
    "LPUSH",
    "RPUSH",
    "LPOP",
    "RPOP",
    "LRANGE",
    "LLEN",
    "LSET",
    "LREM",
    "SADD",
    "SREM",
    "SMEMBERS",
    "SISMEMBER",
    "SCARD",
    "ZADD",
    "ZREM",
    "ZRANGE",
    "ZREVRANGE",
    "ZSCORE",
    "ZCARD",
    "ZINCRBY",
    "XADD",
    "XRANGE",
    "XLEN",
    "INFO",
    "DBSIZE",
    "SELECT",
    "FLUSHDB",
    "PING",
    "OBJECT",
    "MEMORY",
    "CONFIG",
    "CLIENT",
    "PUBLISH",
    "WITHSCORES",
    "MATCH",
    "COUNT",
    "EX",
];

const TABLE_INTRODUCERS: &[&str] = &["FROM", "JOIN", "UPDATE", "INTO", "TABLE"];
const MAX_ITEMS: usize = 40;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
        .to_string()
}

/// Map aliases and table names mentioned after FROM/JOIN/... to table names.
fn referenced_tables(text: &str) -> HashMap<String, String> {
    let tokens: Vec<String> = text
        .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == ')' || c == ';')
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    let mut map = HashMap::new();
    for i in 0..tokens.len() {
        if !TABLE_INTRODUCERS.contains(&tokens[i].to_ascii_uppercase().as_str()) {
            continue;
        }
        let Some(raw) = tokens.get(i + 1) else {
            continue;
        };
        let table = strip_quotes(raw.rsplit('.').next().unwrap_or(raw));
        map.insert(table.to_lowercase(), table.clone());
        let mut j = i + 2;
        if tokens.get(j).is_some_and(|t| t.eq_ignore_ascii_case("AS")) {
            j += 1;
        }
        if let Some(alias) = tokens.get(j) {
            let upper = alias.to_ascii_uppercase();
            if !SQL_WORDS.contains(&upper.as_str()) && alias.chars().all(is_word_char) {
                map.insert(alias.to_lowercase(), table);
            }
        }
    }
    map
}

/// Returns the byte range to replace and the possible completions at a byte cursor.
pub fn candidates(
    kind: DbKind,
    schema: &SchemaCache,
    text: &str,
    cursor: usize,
) -> (usize, Vec<String>) {
    let mut end = cursor.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let start = text[..end]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map_or(end, |(i, _)| i);
    let prefix = &text[start..end];
    let qualifier = text[..start].strip_suffix('.').map(|s| {
        let i = s
            .char_indices()
            .rev()
            .take_while(|(_, c)| is_word_char(*c))
            .last()
            .map_or(s.len(), |(i, _)| i);
        &s[i..]
    });
    let refs = referenced_tables(text);
    let mut out = Vec::<String>::new();
    let mut push = |s: &str| {
        if s.to_lowercase().starts_with(&prefix.to_lowercase()) && !out.iter().any(|x| x == s) {
            out.push(s.into());
        }
    };
    if let Some(q) = qualifier {
        let table = refs.get(&q.to_lowercase()).map_or(q, String::as_str);
        for (t, col) in &schema.columns {
            if t.eq_ignore_ascii_case(table) {
                push(col);
            }
        }
    } else {
        if kind.is_sql() {
            for (t, col) in &schema.columns {
                if refs.values().any(|r| r.eq_ignore_ascii_case(t)) {
                    push(col);
                }
            }
            for t in &schema.tables {
                push(t);
            }
        }
        for w in if kind.is_sql() {
            SQL_WORDS
        } else {
            REDIS_WORDS
        } {
            if prefix.chars().all(|c| !c.is_uppercase()) {
                push(&w.to_lowercase());
            } else {
                push(w);
            }
        }
    }
    out.truncate(MAX_ITEMS);
    (start, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn completes_aliases_and_unicode_offsets() {
        let schema = SchemaCache {
            tables: vec!["users".into()],
            columns: vec![("users".into(), "name".into())],
        };
        let text = "SELECT u.na FROM users AS u";
        assert_eq!(
            candidates(DbKind::Postgres, &schema, text, 11),
            (9, vec!["name".into()])
        );
        let text = "-- café\nSEL";
        let (start, list) = candidates(DbKind::Sqlite, &schema, text, text.len());
        assert_eq!(&text[start..], "SEL");
        assert!(list.contains(&"SELECT".into()));
    }
}
