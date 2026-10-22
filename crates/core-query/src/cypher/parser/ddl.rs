//! Schema statements (TY-9): `CREATE INDEX`, `DROP INDEX`, `SHOW INDEXES`,
//! `CREATE CONSTRAINT`, `DROP CONSTRAINT`, `SHOW CONSTRAINTS`.
//!
//! Every DDL word is an ordinary identifier to the lexer and is matched here
//! by spelling, case-insensitively — the way `REMOVE`, `UNION` and `IS` are —
//! so `index`, `show`, `unique` and the rest still name properties, labels
//! and variables everywhere else. A statement is DDL by its first words only
//! (see [`is_ddl_tokens`](super::is_ddl_tokens)), and it is the whole statement:
//! anything after it is a parse error.

use super::{kw, Parser};
use crate::cypher::ast::DdlStatement;
use crate::cypher::Tok;

impl<'a> Parser<'a> {
    pub(super) fn ddl_statement(&mut self) -> Result<DdlStatement, String> {
        let stmt = if self.eat(&Tok::Create) {
            if self.eat_ident_kw("index") {
                self.create_index()?
            } else if self.eat_ident_kw("constraint") {
                self.create_constraint()?
            } else {
                return Err(self.err("expected INDEX or CONSTRAINT after CREATE"));
            }
        } else if self.eat_ident_kw("drop") {
            if self.eat_ident_kw("index") {
                let (name, if_exists) = self.drop_target("index")?;
                DdlStatement::DropIndex { name, if_exists }
            } else if self.eat_ident_kw("constraint") {
                let (name, if_exists) = self.drop_target("constraint")?;
                DdlStatement::DropConstraint { name, if_exists }
            } else {
                return Err(self.err("expected INDEX or CONSTRAINT after DROP"));
            }
        } else if self.eat_ident_kw("show") {
            if self.eat_ident_kw("indexes") || self.eat_ident_kw("index") {
                DdlStatement::ShowIndexes
            } else if self.eat_ident_kw("constraints") || self.eat_ident_kw("constraint") {
                DdlStatement::ShowConstraints
            } else {
                return Err(self.err("expected INDEXES or CONSTRAINTS after SHOW"));
            }
        } else {
            return Err(self.err("expected CREATE, DROP or SHOW"));
        };
        if self.pos < self.toks.len() {
            return Err(self.err("a schema statement is the whole statement; unexpected tokens"));
        }
        Ok(stmt)
    }

    /// After `CREATE INDEX`: `[name] [IF NOT EXISTS] FOR (n:Label) ON (n.field)`.
    fn create_index(&mut self) -> Result<DdlStatement, String> {
        let name = if self.at_name() {
            Some(self.ident("expected an index name")?)
        } else {
            None
        };
        let if_not_exists = self.if_not_exists()?;
        let (var, label) = self.for_node_pattern()?;
        if !self.eat_ident_kw("on") {
            return Err(self.err("expected ON (var.property)"));
        }
        self.expect(&Tok::LParen, "expected '(' after ON")?;
        let field = self.property_of(&var)?;
        if self.peek() == Some(&Tok::Comma) {
            return Err(
                self.err("an index covers one property; composite indexes are not supported")
            );
        }
        self.expect(&Tok::RParen, "expected ')' after the indexed property")?;
        Ok(DdlStatement::CreateIndex {
            name,
            label,
            field,
            if_not_exists,
        })
    }

    /// After `CREATE CONSTRAINT`:
    /// `name [IF NOT EXISTS] FOR (n:Label) REQUIRE n.field IS UNIQUE`.
    fn create_constraint(&mut self) -> Result<DdlStatement, String> {
        if !self.at_name() {
            return Err(self.err("a constraint needs a name: CREATE CONSTRAINT name FOR …"));
        }
        let name = self.ident("expected a constraint name")?;
        let if_not_exists = self.if_not_exists()?;
        let (var, label) = self.for_node_pattern()?;
        if !self.eat_ident_kw("require") {
            return Err(self.err("expected REQUIRE var.property IS UNIQUE"));
        }
        let parenthesized = self.eat(&Tok::LParen);
        let field = self.property_of(&var)?;
        if parenthesized {
            if self.peek() == Some(&Tok::Comma) {
                return Err(self.err(
                    "a constraint covers one property; composite constraints are not supported",
                ));
            }
            self.expect(&Tok::RParen, "expected ')' after the constrained property")?;
        }
        if !self.eat_ident_kw("is") {
            return Err(self.err("expected IS UNIQUE"));
        }
        if !self.eat_ident_kw("unique") {
            return Err(self.err("only IS UNIQUE constraints are supported"));
        }
        Ok(DdlStatement::CreateConstraint {
            name,
            label,
            field,
            if_not_exists,
        })
    }

    /// After `DROP INDEX` / `DROP CONSTRAINT`: `name [IF EXISTS]`.
    fn drop_target(&mut self, what: &str) -> Result<(String, bool), String> {
        let name = self.ident(&format!("expected the {what} name"))?;
        let if_exists = if self.eat_ident_kw("if") {
            if !self.eat_ident_kw("exists") {
                return Err(self.err("expected EXISTS after IF"));
            }
            true
        } else {
            false
        };
        Ok((name, if_exists))
    }

    /// Whether the next token is a name rather than the `IF NOT EXISTS` or
    /// `FOR (` that follows when there is none. A name spelled `if` or `for`
    /// is still a name when what follows it shows it is one.
    fn at_name(&self) -> bool {
        let next = self.toks.get(self.pos + 1);
        match self.peek() {
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("for") => next != Some(&Tok::LParen),
            Some(Tok::Ident(s)) if s.eq_ignore_ascii_case("if") => next != Some(&Tok::Not),
            Some(Tok::Ident(_)) => true,
            _ => false,
        }
    }

    fn if_not_exists(&mut self) -> Result<bool, String> {
        if !(kw(self.peek(), "if") && self.toks.get(self.pos + 1) == Some(&Tok::Not)) {
            return Ok(false);
        }
        self.pos += 2;
        if !self.eat_ident_kw("exists") {
            return Err(self.err("expected EXISTS after IF NOT"));
        }
        Ok(true)
    }

    /// `FOR (var:Label)`, returning the variable and the label.
    fn for_node_pattern(&mut self) -> Result<(String, String), String> {
        if !self.eat_ident_kw("for") {
            return Err(self.err("expected FOR (var:Label)"));
        }
        self.expect(&Tok::LParen, "expected '(' after FOR")?;
        let var = self.ident("expected a variable")?;
        self.expect(&Tok::Colon, "expected ':' and a label")?;
        let label = self.ident("expected a label")?;
        if self.peek() == Some(&Tok::Colon) {
            return Err(self.err("an index or constraint covers one label"));
        }
        self.expect(&Tok::RParen, "expected ')' after the label")?;
        Ok((var, label))
    }

    /// `var.field`, where `var` must be the pattern's variable.
    fn property_of(&mut self, var: &str) -> Result<String, String> {
        if matches!(self.peek(), Some(Tok::Ident(used)) if used != var) {
            return Err(self.err(&format!("variable is not bound; the pattern binds '{var}'")));
        }
        self.ident("expected var.property")?;
        self.expect(&Tok::Dot, "expected '.' and a property name")?;
        self.ident("expected a property name")
    }
}

#[cfg(test)]
mod tests {
    use crate::cypher::ast::DdlStatement;
    use crate::cypher::{is_ddl_tokens, is_write_tokens, lex, parse_ddl};

    fn ddl(q: &str) -> DdlStatement {
        parse_ddl(&lex(q).unwrap()).unwrap_or_else(|e| panic!("{q}: {e}"))
    }

    fn create_index(name: Option<&str>, label: &str, field: &str, ine: bool) -> DdlStatement {
        DdlStatement::CreateIndex {
            name: name.map(str::to_string),
            label: label.into(),
            field: field.into(),
            if_not_exists: ine,
        }
    }

    #[test]
    fn the_six_shapes_parse() {
        assert_eq!(
            ddl("CREATE INDEX idx_age FOR (n:Person) ON (n.age)"),
            create_index(Some("idx_age"), "Person", "age", false)
        );
        assert_eq!(
            ddl("CREATE INDEX FOR (n:Person) ON (n.score)"),
            create_index(None, "Person", "score", false)
        );
        assert_eq!(
            ddl("create index if not exists for (n:P) on (n.a)"),
            create_index(None, "P", "a", true)
        );
        assert_eq!(
            ddl("CREATE INDEX i IF NOT EXISTS FOR (n:P) ON (n.a)"),
            create_index(Some("i"), "P", "a", true)
        );
        assert_eq!(
            ddl("DROP INDEX i IF EXISTS"),
            DdlStatement::DropIndex {
                name: "i".into(),
                if_exists: true
            }
        );
        assert_eq!(ddl("SHOW INDEXES"), DdlStatement::ShowIndexes);
        assert_eq!(ddl("show index"), DdlStatement::ShowIndexes);
        assert_eq!(
            ddl("CREATE CONSTRAINT uniq_email FOR (n:User) REQUIRE n.email IS UNIQUE"),
            DdlStatement::CreateConstraint {
                name: "uniq_email".into(),
                label: "User".into(),
                field: "email".into(),
                if_not_exists: false,
            }
        );
        assert_eq!(
            ddl("CREATE CONSTRAINT c IF NOT EXISTS FOR (n:U) REQUIRE (n.e) IS UNIQUE"),
            DdlStatement::CreateConstraint {
                name: "c".into(),
                label: "U".into(),
                field: "e".into(),
                if_not_exists: true,
            }
        );
        assert_eq!(
            ddl("DROP CONSTRAINT c"),
            DdlStatement::DropConstraint {
                name: "c".into(),
                if_exists: false
            }
        );
        assert_eq!(ddl("SHOW CONSTRAINTS"), DdlStatement::ShowConstraints);
    }

    #[test]
    fn names_spelled_like_the_keywords_around_them_are_names() {
        assert_eq!(
            ddl("CREATE INDEX for FOR (n:P) ON (n.a)"),
            create_index(Some("for"), "P", "a", false)
        );
        assert_eq!(
            ddl("CREATE INDEX if FOR (n:P) ON (n.a)"),
            create_index(Some("if"), "P", "a", false)
        );
        assert_eq!(
            ddl("CREATE INDEX `my idx` FOR (index:Index) ON (index.index)"),
            create_index(Some("my idx"), "Index", "index", false)
        );
    }

    #[test]
    fn anything_but_the_statement_is_an_error() {
        for q in [
            "CREATE INDEX i FOR (n:P) ON (n.a) RETURN 1",
            "CREATE INDEX i FOR (n:P) ON (m.a)",
            "CREATE INDEX i FOR (n:P) ON (n.a, n.b)",
            "CREATE INDEX i FOR (n:P:Q) ON (n.a)",
            "CREATE INDEX i ON (n.a)",
            "CREATE CONSTRAINT FOR (n:P) REQUIRE n.a IS UNIQUE",
            "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS NOT NULL",
            "CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a",
            "DROP INDEX",
            "DROP INDEX i IF",
            "DROP TABLE t",
            "SHOW TABLES",
            "SHOW INDEXES YIELD name",
        ] {
            assert!(parse_ddl(&lex(q).unwrap()).is_err(), "{q}");
        }
    }

    #[test]
    fn ddl_is_told_by_its_first_words() {
        for q in [
            "CREATE INDEX i FOR (n:P) ON (n.a)",
            "create constraint c for (n:P) require n.a is unique",
            "DROP INDEX i",
            "drop anything",
            "SHOW INDEXES",
            "show anything",
        ] {
            assert!(is_ddl_tokens(&lex(q).unwrap()), "{q}");
        }
        for q in [
            "CREATE (n:Index {id: 'a'})",
            "MATCH (show) RETURN show.index",
            "MATCH (n) SET n.drop = 1",
            "RETURN 1",
        ] {
            assert!(!is_ddl_tokens(&lex(q).unwrap()), "{q}");
        }
    }

    #[test]
    fn create_and_drop_are_writes_and_show_is_a_read() {
        let write = |q: &str| is_write_tokens(&lex(q).unwrap());
        assert!(write("CREATE INDEX i FOR (n:P) ON (n.a)"));
        assert!(write("CREATE CONSTRAINT c FOR (n:P) REQUIRE n.a IS UNIQUE"));
        assert!(write("DROP INDEX i"));
        assert!(write("DROP CONSTRAINT c"));
        assert!(!write("SHOW INDEXES"));
        assert!(!write("SHOW CONSTRAINTS"));
        assert!(!write("MATCH (n) RETURN n.drop"));
    }
}
