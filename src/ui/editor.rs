//! SQL statement splitting independent of the UI toolkit.

/// Character ranges `[start, end)` of the statements in `text`, split on `;` outside of quotes,
/// comments and PostgreSQL dollar-quoted strings. The `;` itself is not included.
pub fn statement_ranges(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < n {
        let c = chars[i];
        match c {
            '\'' | '"' | '`' => {
                i += 1;
                while i < n && chars[i] != c {
                    if chars[i] == '\\' && c != '"' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            '-' if chars.get(i + 1) == Some(&'-') => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < n && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 1;
            }
            '$' => {
                // $tag$ ... $tag$
                let mut j = i + 1;
                while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                    j += 1;
                }
                if j < n && chars[j] == '$' && !(j > i + 1 && chars[i + 1].is_ascii_digit()) {
                    let tag: Vec<char> = chars[i..=j].to_vec();
                    i = j + 1;
                    while i < n && !chars[i..].starts_with(&tag) {
                        i += 1;
                    }
                    i += tag.len() - 1;
                }
            }
            ';' => {
                ranges.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < n {
        ranges.push((start, n));
    }
    ranges
}

/// The statement under the cursor (character offset). A cursor right after `;` or on blank
/// lines following a statement selects that statement.
pub fn statement_at(text: &str, cursor: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let is_blank = |(s, e): (usize, usize)| chars[s..e].iter().all(|c| c.is_whitespace());
    let ranges: Vec<(usize, usize)> = statement_ranges(text);
    let idx = ranges
        .iter()
        .position(|&(s, e)| cursor >= s && cursor <= e)
        .unwrap_or(ranges.len());
    // Walk back over empty statements (e.g. the cursor sits after the final `;`).
    let mut i = idx.min(ranges.len());
    loop {
        if let Some(&r) = ranges.get(i)
            && !is_blank(r)
        {
            return Some(r);
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Plain,
    Keyword,
    String,
    Number,
    Comment,
}

/// Highlighted runs as `(line, column, text, kind)`, split at line ends. Columns count chars.
pub fn highlight(text: &str) -> Vec<(usize, usize, String, Token)> {
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut kinds = vec![Token::Plain; n];
    let mut i = 0;
    while i < n {
        let start = i;
        let c = chars[i];
        let kind = if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            Token::Comment
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < n && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(n);
            Token::Comment
        } else if c == '\'' {
            i += 1;
            while i < n && chars[i] != '\'' {
                i += if chars[i] == '\\' { 2 } else { 1 };
            }
            i = (i + 1).min(n);
            Token::String
        } else if c == '$' {
            let mut j = i + 1;
            while j < n && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            if j < n && chars[j] == '$' && !(j > i + 1 && chars[i + 1].is_ascii_digit()) {
                let tag = &chars[i..=j];
                i = j + 1;
                while i < n && !chars[i..].starts_with(tag) {
                    i += 1;
                }
                i = (i + tag.len()).min(n);
                Token::String
            } else {
                i = j.max(i + 1);
                Token::Plain
            }
        } else if c.is_ascii_digit() {
            while i < n && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            Token::Number
        } else if c.is_alphanumeric() || c == '_' {
            while i < n && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect::<String>().to_uppercase();
            if super::completion::SQL_WORDS.contains(&word.as_str()) {
                Token::Keyword
            } else {
                Token::Plain
            }
        } else if c == '"' || c == '`' {
            i += 1;
            while i < n && chars[i] != c {
                i += 1;
            }
            i = (i + 1).min(n);
            Token::Plain
        } else {
            i += 1;
            Token::Plain
        };
        kinds[start..i].fill(kind);
    }
    let mut runs = Vec::new();
    let (mut line, mut col) = (0, 0);
    let mut i = 0;
    while i < n {
        if chars[i] == '\n' {
            line += 1;
            col = 0;
            i += 1;
            continue;
        }
        if chars[i].is_whitespace() {
            col += 1;
            i += 1;
            continue;
        }
        let start = i;
        while i < n && chars[i] != '\n' && kinds[i] == kinds[start] {
            i += 1;
        }
        let run: String = chars[start..i].iter().collect();
        let trimmed = run.trim_end();
        runs.push((line, col, trimmed.to_string(), kinds[start]));
        col += i - start;
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(text: &str, cursor: usize) -> String {
        let (s, e) = statement_at(text, cursor).unwrap();
        text.chars()
            .skip(s)
            .take(e - s)
            .collect::<String>()
            .trim()
            .to_string()
    }

    #[test]
    fn splits_statements() {
        let sql = "SELECT ';' AS a; -- x;y\nSELECT 2;\n\n";
        assert_eq!(statement_ranges(sql).len(), 3);
        assert_eq!(stmt(sql, 3), "SELECT ';' AS a");
        assert_eq!(stmt(sql, 26), "-- x;y\nSELECT 2");
        assert_eq!(stmt(sql, sql.chars().count()), "-- x;y\nSELECT 2");
        let func = "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql; SELECT $1";
        assert_eq!(statement_ranges(func).len(), 2);
        assert_eq!(stmt("  SELECT 1", 0), "SELECT 1");
        assert!(statement_at("  \n ", 1).is_none());
    }

    #[test]
    fn highlights_tokens() {
        let runs = highlight("select 'a b', 42 -- hi\n  /* x\ny */ foo");
        let kinds: Vec<_> = runs.iter().map(|r| (r.0, r.1, r.2.as_str(), r.3)).collect();
        assert_eq!(
            kinds,
            vec![
                (0, 0, "select", Token::Keyword),
                (0, 7, "'a b'", Token::String),
                (0, 12, ",", Token::Plain),
                (0, 14, "42", Token::Number),
                (0, 17, "-- hi", Token::Comment),
                (1, 2, "/* x", Token::Comment),
                (2, 0, "y */", Token::Comment),
                (2, 5, "foo", Token::Plain),
            ]
        );
    }
}
