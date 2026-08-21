//! Cypher subset lexer. Never panics on any `&str` input.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // keywords (case-insensitive source, canonical here)
    Match,
    Where,
    Return,
    Order,
    By,
    Skip,
    Limit,
    As,
    And,
    Or,
    Not,
    Asc,
    Desc,
    // pipeline keywords
    With,
    Unwind,
    /// `OPTIONAL` — marks the start of an `OPTIONAL MATCH` clause.
    Optional,
    // write keywords
    Create,
    Set,
    Delete,
    Detach,
    Merge,
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Param(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dot,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Dash, // BINDING: the lexer emits `Dash`, `Lt`, `Gt` as separate tokens and the
    // PARSER assembles rel-arrow shapes (`-[..]->`, `<-[..]-`, `-[..]-`);
    // `<=`, `>=`, `<>` are single tokens (Le, Ge, Ne).
    /// `*` — used in `COUNT(*)`.
    Star,
}

pub fn lex(input: &str) -> Result<Vec<Tok>, String> {
    let mut toks = Vec::new();
    let mut chars = input.char_indices().peekable();
    while let Some((i, ch)) = chars.next() {
        match ch {
            c if c.is_whitespace() => {}
            '(' => toks.push(Tok::LParen),
            ')' => toks.push(Tok::RParen),
            '[' => toks.push(Tok::LBracket),
            ']' => toks.push(Tok::RBracket),
            '{' => toks.push(Tok::LBrace),
            '}' => toks.push(Tok::RBrace),
            ':' => toks.push(Tok::Colon),
            ',' => toks.push(Tok::Comma),
            '.' => toks.push(Tok::Dot),
            '=' => toks.push(Tok::Eq),
            '*' => toks.push(Tok::Star),
            '-' => toks.push(Tok::Dash),
            '<' => match chars.peek() {
                Some((_, '=')) => {
                    chars.next();
                    toks.push(Tok::Le);
                }
                Some((_, '>')) => {
                    chars.next();
                    toks.push(Tok::Ne);
                }
                _ => toks.push(Tok::Lt),
            },
            '>' => {
                if matches!(chars.peek(), Some((_, '='))) {
                    chars.next();
                    toks.push(Tok::Ge);
                } else {
                    toks.push(Tok::Gt);
                }
            }
            '\'' => toks.push(lex_string(i, &mut chars)?),
            '$' => toks.push(lex_param(input, i, &mut chars)?),
            '0'..='9' => toks.push(lex_number(input, i, ch, &mut chars)?),
            'A'..='Z' | 'a'..='z' | '_' => toks.push(lex_word(input, i, ch, &mut chars)),
            _ => return Err(format!("illegal character {ch:?} at position {i}")),
        }
    }
    Ok(toks)
}

fn lex_string(
    start: usize,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Result<Tok, String> {
    let mut out = String::new();
    loop {
        match chars.next() {
            None => return Err(format!("unterminated string at position {start}")),
            Some((_, '\'')) => return Ok(Tok::Str(out)),
            Some((_, '\\')) => match chars.next() {
                Some((_, '\'')) => out.push('\''),
                Some((pos, ch)) => {
                    return Err(format!("invalid escape '\\{ch}' at position {pos}"));
                }
                None => return Err(format!("unterminated string at position {start}")),
            },
            Some((_, ch)) => out.push(ch),
        }
    }
}

fn lex_param(
    input: &str,
    start: usize,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Result<Tok, String> {
    match chars.peek() {
        Some((_, ch)) if is_ident_start(*ch) => Ok(Tok::Param(take_ident(input, chars))),
        _ => Err(format!("invalid parameter at position {start}")),
    }
}

fn lex_number(
    input: &str,
    start: usize,
    first: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Result<Tok, String> {
    let mut end = start + first.len_utf8();
    while let Some(&(p, ch)) = chars.peek() {
        if ch.is_ascii_digit() {
            chars.next();
            end = p + ch.len_utf8();
        } else {
            break;
        }
    }
    if let Some(&(dot_pos, '.')) = chars.peek() {
        let after_dot = input[dot_pos + '.'.len_utf8()..].chars().next();
        if after_dot.is_some_and(|c| c.is_ascii_digit()) {
            chars.next();
            end = dot_pos + '.'.len_utf8();
            while let Some(&(p, ch)) = chars.peek() {
                if ch.is_ascii_digit() {
                    chars.next();
                    end = p + ch.len_utf8();
                } else {
                    break;
                }
            }
            let val: f64 = input[start..end]
                .parse()
                .map_err(|_| format!("invalid float at position {start}"))?;
            return Ok(Tok::Float(val));
        }
        // `..` (two consecutive dots) is the range separator for variable-length
        // path patterns (`*1..5`).  When after_dot is also '.', stop the number
        // here without consuming the first dot — the main lex loop will emit two
        // `Dot` tokens for the `..` separator.
        if after_dot == Some('.') {
            // fall through to return the integer below
        } else {
            return Err(format!(
                "invalid number at position {start}: expected digit after decimal point"
            ));
        }
    }
    let val: i64 = input[start..end]
        .parse()
        .map_err(|_| format!("invalid integer at position {start}"))?;
    Ok(Tok::Int(val))
}

fn lex_word(
    input: &str,
    start: usize,
    first: char,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Tok {
    let mut end = start + first.len_utf8();
    while let Some(&(p, ch)) = chars.peek() {
        if is_ident_cont(ch) {
            chars.next();
            end = p + ch.len_utf8();
        } else {
            break;
        }
    }
    keyword(&input[start..end]).unwrap_or_else(|| Tok::Ident(input[start..end].to_string()))
}

fn take_ident(input: &str, chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> String {
    let (start, first) = match chars.next() {
        Some(pair) => pair,
        None => return String::new(),
    };
    let mut end = start + first.len_utf8();
    while let Some(&(p, ch)) = chars.peek() {
        if is_ident_cont(ch) {
            chars.next();
            end = p + ch.len_utf8();
        } else {
            break;
        }
    }
    input[start..end].to_string()
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_ident_cont(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn keyword(word: &str) -> Option<Tok> {
    Some(match word.to_ascii_lowercase().as_str() {
        "match" => Tok::Match,
        "where" => Tok::Where,
        "return" => Tok::Return,
        "order" => Tok::Order,
        "by" => Tok::By,
        "skip" => Tok::Skip,
        "limit" => Tok::Limit,
        "as" => Tok::As,
        "and" => Tok::And,
        "or" => Tok::Or,
        "not" => Tok::Not,
        "asc" => Tok::Asc,
        "desc" => Tok::Desc,
        "with" => Tok::With,
        "unwind" => Tok::Unwind,
        "optional" => Tok::Optional,
        "create" => Tok::Create,
        "set" => Tok::Set,
        "delete" => Tok::Delete,
        "detach" => Tok::Detach,
        "merge" => Tok::Merge,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{lex, Tok};

    #[test]
    fn keywords_are_case_insensitive() {
        let expected = vec![Tok::Match];
        assert_eq!(lex("MATCH").unwrap(), expected);
        assert_eq!(lex("match").unwrap(), expected);
        assert_eq!(lex("MaTcH").unwrap(), expected);
        assert_eq!(
            lex("WHERE RETURN ORDER BY SKIP LIMIT AS AND OR NOT ASC DESC").unwrap(),
            vec![
                Tok::Where,
                Tok::Return,
                Tok::Order,
                Tok::By,
                Tok::Skip,
                Tok::Limit,
                Tok::As,
                Tok::And,
                Tok::Or,
                Tok::Not,
                Tok::Asc,
                Tok::Desc,
            ]
        );
        assert_eq!(
            lex("where return order by skip limit as and or not asc desc").unwrap(),
            vec![
                Tok::Where,
                Tok::Return,
                Tok::Order,
                Tok::By,
                Tok::Skip,
                Tok::Limit,
                Tok::As,
                Tok::And,
                Tok::Or,
                Tok::Not,
                Tok::Asc,
                Tok::Desc,
            ]
        );
    }

    #[test]
    fn comparison_ops_disambiguate() {
        assert_eq!(lex("<").unwrap(), vec![Tok::Lt]);
        assert_eq!(lex("<=").unwrap(), vec![Tok::Le]);
        assert_eq!(lex("<>").unwrap(), vec![Tok::Ne]);
        assert_eq!(lex(">").unwrap(), vec![Tok::Gt]);
        assert_eq!(lex(">=").unwrap(), vec![Tok::Ge]);
        assert_eq!(lex("=").unwrap(), vec![Tok::Eq]);
        assert_eq!(
            lex("< <= <> > >=").unwrap(),
            vec![Tok::Lt, Tok::Le, Tok::Ne, Tok::Gt, Tok::Ge]
        );
    }

    #[test]
    fn string_escape_apostrophe() {
        assert_eq!(lex(r"'it\'s'").unwrap(), vec![Tok::Str("it's".into())]);
    }

    #[test]
    fn float_vs_int_vs_bare_dot_is_error() {
        assert_eq!(lex("42").unwrap(), vec![Tok::Int(42)]);
        assert_eq!(lex("2.5").unwrap(), vec![Tok::Float(2.5)]);
        let err = lex("1.").expect_err("digit-dot with no following digit is an error");
        assert!(
            err.contains("position"),
            "error must include position info, got: {err}"
        );
    }

    #[test]
    fn dollar_param() {
        assert_eq!(lex("$name").unwrap(), vec![Tok::Param("name".into())]);
        assert_eq!(lex("$tid").unwrap(), vec![Tok::Param("tid".into())]);
    }

    #[test]
    fn unterminated_string_is_err_with_position() {
        let err = lex("'abc").expect_err("unterminated string must be Err");
        assert!(
            err.contains("position"),
            "error must include position info, got: {err}"
        );
        let err = lex("MATCH 'oops").expect_err("unterminated after tokens");
        assert!(
            err.contains("position"),
            "error must include position info, got: {err}"
        );
    }

    #[test]
    fn composite_query_emits_every_token_variant() {
        // Every Tok variant appears at least once. Rel arrows stay as Dash/Lt/Gt.
        let src = r"MATCH (n:L {k: 'it\'s', i: 1, f: 2.5})-[r]->(m) WHERE NOT n.a = $p AND n.b <> 0 OR n.c < 1 AND n.d <= 2 AND n.e > 3 AND n.f >= 4.0 RETURN n, n.a AS x ORDER BY x ASC, n.b DESC SKIP 0 LIMIT 10";
        assert_eq!(
            lex(src).unwrap(),
            vec![
                Tok::Match,
                Tok::LParen,
                Tok::Ident("n".into()),
                Tok::Colon,
                Tok::Ident("L".into()),
                Tok::LBrace,
                Tok::Ident("k".into()),
                Tok::Colon,
                Tok::Str("it's".into()),
                Tok::Comma,
                Tok::Ident("i".into()),
                Tok::Colon,
                Tok::Int(1),
                Tok::Comma,
                Tok::Ident("f".into()),
                Tok::Colon,
                Tok::Float(2.5),
                Tok::RBrace,
                Tok::RParen,
                Tok::Dash,
                Tok::LBracket,
                Tok::Ident("r".into()),
                Tok::RBracket,
                Tok::Dash,
                Tok::Gt,
                Tok::LParen,
                Tok::Ident("m".into()),
                Tok::RParen,
                Tok::Where,
                Tok::Not,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("a".into()),
                Tok::Eq,
                Tok::Param("p".into()),
                Tok::And,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("b".into()),
                Tok::Ne,
                Tok::Int(0),
                Tok::Or,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("c".into()),
                Tok::Lt,
                Tok::Int(1),
                Tok::And,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("d".into()),
                Tok::Le,
                Tok::Int(2),
                Tok::And,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("e".into()),
                Tok::Gt,
                Tok::Int(3),
                Tok::And,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("f".into()),
                Tok::Ge,
                Tok::Float(4.0),
                Tok::Return,
                Tok::Ident("n".into()),
                Tok::Comma,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("a".into()),
                Tok::As,
                Tok::Ident("x".into()),
                Tok::Order,
                Tok::By,
                Tok::Ident("x".into()),
                Tok::Asc,
                Tok::Comma,
                Tok::Ident("n".into()),
                Tok::Dot,
                Tok::Ident("b".into()),
                Tok::Desc,
                Tok::Skip,
                Tok::Int(0),
                Tok::Limit,
                Tok::Int(10),
            ]
        );
    }

    #[test]
    fn garbage_bytes_are_err_not_panic() {
        let cases = ["@", "#", "MATCH @ n", "\"double\"", "\u{0}", "1.2.3@"];
        for src in cases {
            let result = std::panic::catch_unwind(|| lex(src));
            assert!(result.is_ok(), "lex({src:?}) panicked");
            let err = result
                .unwrap()
                .expect_err(&format!("lex({src:?}) must be Err"));
            assert!(
                err.contains("position"),
                "error must include position info, got: {err}"
            );
        }
    }
}
