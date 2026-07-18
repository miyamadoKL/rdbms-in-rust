//! Token列をASTへ変換する構文解析器。
//!
//! 文(`SELECT` / `CREATE TABLE` / `INSERT INTO`)はRecursive Descentで、式は
//! Pratt Parser(binding power)で解析する。使い分けの理由は本文([第7章](../book))
//! を参照。
//!
//! 対応する構文は次のとおり。
//! - `SELECT <式> [, <式> ...] [FROM <table>] [WHERE <式>]`
//! - `CREATE TABLE <table> (<col> <type> [NOT NULL], ...)`
//! - `INSERT INTO <table> VALUES (<式>, ...)`
//! - 式: リテラル(整数・文字列・真偽値・`NULL`)、列参照、二項演算(`+ - * /`、
//!   比較、`AND` `OR`)、単項演算(`-` `NOT`)、`IS [NOT] NULL`、関数呼び出し、
//!   `CAST(expr AS type)`、括弧
//!
//! 優先順位は低い順に`OR` < `AND` < `NOT` < 比較 < `+` `-` < `*` `/` < 単項`-`。

use crate::ast::{
    BinaryOperator, ColumnDef, CreateTableStatement, Expr, Ident, InsertStatement, SelectItem,
    SelectStatement, Statement, UnaryOperator,
};
use crate::error::{DbError, DbResult};
use crate::lexer::{self, Keyword, Span, Token, TokenKind};

/// SQL文字列を1本のASTへ変換する。
///
/// 字句解析(`lexer::tokenize`)が失敗した場合は`DbError::Lex`を、Token列は
/// 得られたがこのサブセットの文法に適合しない場合は`DbError::Parse`を返す。
/// 末尾のセミコロンは省略でき、あっても無くても同じASTになる。
pub fn parse_statement(source: &str) -> DbResult<Statement> {
    let tokens = lexer::tokenize(source)?;
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
    };
    let statement = parser.parse_statement()?;
    parser.expect_end()?;
    Ok(statement)
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> &Token {
        // `tokenize`が返すTokenの末尾は必ずEofなので、`pos`が配列末尾を
        // 超えることはない。
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.peek().kind
    }

    fn advance(&mut self) -> Token {
        let token = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    fn error_at(&self, span: Span, message: impl Into<String>) -> DbError {
        let (line, column) = lexer::line_col(self.source, span.start);
        DbError::Parse {
            message: message.into(),
            line,
            column,
        }
    }

    /// 現在のトークンについてのエラーを作る。「何を期待していたか」を添える。
    fn unexpected(&self, expected: &str) -> DbError {
        let token = self.peek();
        self.error_at(
            token.span,
            format!("{expected}が必要です: {:?}が見つかりました", token.kind),
        )
    }

    fn expect_keyword(&mut self, keyword: Keyword, label: &str) -> DbResult<Span> {
        if let TokenKind::Keyword(k) = self.peek_kind()
            && *k == keyword
        {
            Ok(self.advance().span)
        } else {
            Err(self.unexpected(label))
        }
    }

    fn expect_punct(&mut self, expected: TokenKind, label: &str) -> DbResult<Span> {
        if *self.peek_kind() == expected {
            Ok(self.advance().span)
        } else {
            Err(self.unexpected(label))
        }
    }

    fn expect_ident(&mut self) -> DbResult<Ident> {
        match self.peek_kind() {
            TokenKind::Ident(name) => {
                let name = name.clone();
                let span = self.advance().span;
                Ok(Ident { name, span })
            }
            _ => Err(self.unexpected("識別子")),
        }
    }

    /// 文の解析後、末尾の`;`(あれば)を読み飛ばしたうえで、残りが`Eof`だけに
    /// なっていることを確認する。余分なトークンが残っていれば構文エラーとする。
    fn expect_end(&mut self) -> DbResult<()> {
        if *self.peek_kind() == TokenKind::Semicolon {
            self.advance();
        }
        if *self.peek_kind() == TokenKind::Eof {
            Ok(())
        } else {
            Err(self.unexpected("文の終端"))
        }
    }

    fn parse_statement(&mut self) -> DbResult<Statement> {
        match self.peek_kind() {
            TokenKind::Keyword(Keyword::Select) => {
                self.parse_select_statement().map(Statement::Select)
            }
            TokenKind::Keyword(Keyword::Create) => self
                .parse_create_table_statement()
                .map(Statement::CreateTable),
            TokenKind::Keyword(Keyword::Insert) => {
                self.parse_insert_statement().map(Statement::Insert)
            }
            _ => Err(self.unexpected("SELECT・CREATE TABLE・INSERT INTOのいずれか")),
        }
    }

    // ---- SELECT ----

    fn parse_select_statement(&mut self) -> DbResult<SelectStatement> {
        let start = self.expect_keyword(Keyword::Select, "SELECT")?.start;

        let mut items = vec![self.parse_select_item()?];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            items.push(self.parse_select_item()?);
        }
        let mut end = items.last().expect("直前にpushしたばかり").span.end;

        let from = if let TokenKind::Keyword(Keyword::From) = self.peek_kind() {
            self.advance();
            let table = self.expect_ident()?;
            end = table.span.end;
            Some(table)
        } else {
            None
        };

        let where_clause = if let TokenKind::Keyword(Keyword::Where) = self.peek_kind() {
            self.advance();
            let expr = self.parse_expr(0)?;
            end = expr.span().end;
            Some(expr)
        } else {
            None
        };

        Ok(SelectStatement {
            items,
            from,
            where_clause,
            span: Span::new(start, end),
        })
    }

    fn parse_select_item(&mut self) -> DbResult<SelectItem> {
        let expr = self.parse_expr(0)?;
        let span = expr.span();
        Ok(SelectItem { expr, span })
    }

    // ---- CREATE TABLE ----

    fn parse_create_table_statement(&mut self) -> DbResult<CreateTableStatement> {
        let start = self.expect_keyword(Keyword::Create, "CREATE")?.start;
        self.expect_keyword(Keyword::Table, "TABLE")?;
        let table = self.expect_ident()?;
        self.expect_punct(TokenKind::LParen, "(")?;

        let mut columns = vec![self.parse_column_def()?];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            columns.push(self.parse_column_def()?);
        }

        let end = self.expect_punct(TokenKind::RParen, ")")?.end;

        Ok(CreateTableStatement {
            table,
            columns,
            span: Span::new(start, end),
        })
    }

    fn parse_column_def(&mut self) -> DbResult<ColumnDef> {
        let name = self.expect_ident()?;
        let type_name = self.expect_ident()?;
        let mut end = type_name.span.end;

        let not_null = if let TokenKind::Keyword(Keyword::Not) = self.peek_kind() {
            self.advance();
            end = self.expect_keyword(Keyword::Null, "NULL")?.end;
            true
        } else {
            false
        };

        Ok(ColumnDef {
            span: Span::new(name.span.start, end),
            name,
            type_name,
            not_null,
        })
    }

    // ---- INSERT INTO ----

    fn parse_insert_statement(&mut self) -> DbResult<InsertStatement> {
        let start = self.expect_keyword(Keyword::Insert, "INSERT")?.start;
        self.expect_keyword(Keyword::Into, "INTO")?;
        let table = self.expect_ident()?;
        self.expect_keyword(Keyword::Values, "VALUES")?;
        self.expect_punct(TokenKind::LParen, "(")?;

        let mut values = vec![self.parse_expr(0)?];
        while *self.peek_kind() == TokenKind::Comma {
            self.advance();
            values.push(self.parse_expr(0)?);
        }

        let end = self.expect_punct(TokenKind::RParen, ")")?.end;

        Ok(InsertStatement {
            table,
            values,
            span: Span::new(start, end),
        })
    }

    // ---- 式(Pratt Parser) ----

    /// `min_bp`以上のbinding powerを持つ演算子だけを、このループが自分で消費する。
    /// `min_bp`未満の演算子に出会ったら、それは呼び出し元(より外側の演算子や
    /// 文レベルの解析)が消費すべきものなので、消費せずに返る。
    fn parse_expr(&mut self, min_bp: u8) -> DbResult<Expr> {
        let mut lhs = self.parse_prefix()?;

        loop {
            if let TokenKind::Keyword(Keyword::Is) = self.peek_kind() {
                const IS_NULL_BP: u8 = 6;
                if IS_NULL_BP < min_bp {
                    break;
                }
                self.advance();
                let negated = if let TokenKind::Keyword(Keyword::Not) = self.peek_kind() {
                    self.advance();
                    true
                } else {
                    false
                };
                let end = self.expect_keyword(Keyword::Null, "NULL")?.end;
                let span = Span::new(lhs.span().start, end);
                lhs = Expr::IsNull {
                    expr: Box::new(lhs),
                    negated,
                    span,
                };
                continue;
            }

            let Some((op, lbp, rbp)) = infix_binding_power(self.peek_kind()) else {
                break;
            };
            if lbp < min_bp {
                break;
            }
            self.advance();
            let rhs = self.parse_expr(rbp)?;
            let span = Span::new(lhs.span().start, rhs.span().end);
            lhs = Expr::BinaryOp {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }

        Ok(lhs)
    }

    /// 前置演算子(`NOT`、単項`-`)を読み、無ければ`parse_primary`に委ねる。
    fn parse_prefix(&mut self) -> DbResult<Expr> {
        const NOT_RBP: u8 = 5;
        const NEGATE_RBP: u8 = 12;

        match self.peek_kind() {
            TokenKind::Keyword(Keyword::Not) => {
                let start = self.advance().span.start;
                let operand = self.parse_expr(NOT_RBP)?;
                let span = Span::new(start, operand.span().end);
                Ok(Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(operand),
                    span,
                })
            }
            TokenKind::Minus => {
                let start = self.advance().span.start;
                let operand = self.parse_expr(NEGATE_RBP)?;
                let span = Span::new(start, operand.span().end);
                Ok(Expr::UnaryOp {
                    op: UnaryOperator::Negate,
                    expr: Box::new(operand),
                    span,
                })
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> DbResult<Expr> {
        match self.peek_kind().clone() {
            TokenKind::IntLiteral(value) => {
                let span = self.advance().span;
                Ok(Expr::IntLiteral { value, span })
            }
            TokenKind::StringLiteral(value) => {
                let span = self.advance().span;
                Ok(Expr::StringLiteral { value, span })
            }
            TokenKind::Keyword(Keyword::True) => {
                let span = self.advance().span;
                Ok(Expr::BoolLiteral { value: true, span })
            }
            TokenKind::Keyword(Keyword::False) => {
                let span = self.advance().span;
                Ok(Expr::BoolLiteral { value: false, span })
            }
            TokenKind::Keyword(Keyword::Null) => {
                let span = self.advance().span;
                Ok(Expr::NullLiteral { span })
            }
            TokenKind::Ident(name) => {
                let start_span = self.advance().span;
                if *self.peek_kind() == TokenKind::LParen {
                    self.parse_function_call(name, start_span)
                } else {
                    Ok(Expr::ColumnRef {
                        name,
                        span: start_span,
                    })
                }
            }
            TokenKind::LParen => {
                let start = self.advance().span.start;
                let inner = self.parse_expr(0)?;
                let end = self.expect_punct(TokenKind::RParen, ")")?.end;
                Ok(Expr::Paren {
                    expr: Box::new(inner),
                    span: Span::new(start, end),
                })
            }
            TokenKind::Keyword(Keyword::Cast) => self.parse_cast(),
            _ => Err(self.unexpected("式")),
        }
    }

    /// `CAST(expr AS type_name)`を読む。`type_name`は`CREATE TABLE`の列定義と
    /// 同じく、型名の一覧と突き合わせずに`Ident`のまま保持する。
    fn parse_cast(&mut self) -> DbResult<Expr> {
        let start = self.expect_keyword(Keyword::Cast, "CAST")?.start;
        self.expect_punct(TokenKind::LParen, "(")?;
        let expr = self.parse_expr(0)?;
        self.expect_keyword(Keyword::As, "AS")?;
        let type_name = self.expect_ident()?;
        let end = self.expect_punct(TokenKind::RParen, ")")?.end;
        Ok(Expr::Cast {
            expr: Box::new(expr),
            type_name,
            span: Span::new(start, end),
        })
    }

    fn parse_function_call(&mut self, name: String, name_span: Span) -> DbResult<Expr> {
        self.expect_punct(TokenKind::LParen, "(")?;

        let mut args = Vec::new();
        if *self.peek_kind() != TokenKind::RParen {
            args.push(self.parse_expr(0)?);
            while *self.peek_kind() == TokenKind::Comma {
                self.advance();
                args.push(self.parse_expr(0)?);
            }
        }

        let end = self.expect_punct(TokenKind::RParen, ")")?.end;
        Ok(Expr::FunctionCall {
            name,
            args,
            span: Span::new(name_span.start, end),
        })
    }
}

/// 二項演算子のbinding power(左結合力, 右結合力)。
///
/// 左結合力(`lbp`)は「この演算子を今のループで消費してよいか」の判定に、
/// 右結合力(`rbp`)は「右辺を解析する再帰呼び出しに渡す`min_bp`」に使う。
/// 左結合の演算子は`rbp = lbp + 1`にすることで、`1 - 2 - 3`が`(1 - 2) - 3`に
/// なる(右辺の再帰呼び出しが同じ優先順位の演算子を自分では取り込まない)。
///
/// 優先順位は低い順に`OR`(1) < `AND`(3) < 比較(6) < `+` `-`(8) < `*` `/`(10)。
/// `NOT`(前置, rbp=5)と`IS [NOT] NULL`(lbp=6)は`parse_expr`側で扱う。
fn infix_binding_power(kind: &TokenKind) -> Option<(BinaryOperator, u8, u8)> {
    let (op, lbp) = match kind {
        TokenKind::Keyword(Keyword::Or) => (BinaryOperator::Or, 1),
        TokenKind::Keyword(Keyword::And) => (BinaryOperator::And, 3),
        TokenKind::Eq => (BinaryOperator::Eq, 6),
        TokenKind::NotEq => (BinaryOperator::NotEq, 6),
        TokenKind::Lt => (BinaryOperator::Lt, 6),
        TokenKind::LtEq => (BinaryOperator::LtEq, 6),
        TokenKind::Gt => (BinaryOperator::Gt, 6),
        TokenKind::GtEq => (BinaryOperator::GtEq, 6),
        TokenKind::Plus => (BinaryOperator::Add, 8),
        TokenKind::Minus => (BinaryOperator::Subtract, 8),
        TokenKind::Star => (BinaryOperator::Multiply, 10),
        TokenKind::Slash => (BinaryOperator::Divide, 10),
        _ => return None,
    };
    Some((op, lbp, lbp + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Statement;

    fn parse_expr_only(sql: &str) -> Expr {
        match parse_statement(&format!("SELECT {sql}")).unwrap() {
            Statement::Select(select) => {
                assert_eq!(select.items.len(), 1, "式は1個だけのはず");
                select.items.into_iter().next().unwrap().expr
            }
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    fn int(v: i64) -> Box<Expr> {
        Box::new(Expr::IntLiteral {
            value: v,
            span: Span::new(0, 0),
        })
    }

    /// テストではSpanの中身までは比較しない(位置は別のテストで確認する)ので、
    /// すべてのSpanをダミー値に潰してから比較する。
    fn strip_spans(expr: Expr) -> Expr {
        let dummy = Span::new(0, 0);
        match expr {
            Expr::IntLiteral { value, .. } => Expr::IntLiteral { value, span: dummy },
            Expr::StringLiteral { value, .. } => Expr::StringLiteral { value, span: dummy },
            Expr::BoolLiteral { value, .. } => Expr::BoolLiteral { value, span: dummy },
            Expr::NullLiteral { .. } => Expr::NullLiteral { span: dummy },
            Expr::ColumnRef { name, .. } => Expr::ColumnRef { name, span: dummy },
            Expr::UnaryOp { op, expr, .. } => Expr::UnaryOp {
                op,
                expr: Box::new(strip_spans(*expr)),
                span: dummy,
            },
            Expr::BinaryOp { op, lhs, rhs, .. } => Expr::BinaryOp {
                op,
                lhs: Box::new(strip_spans(*lhs)),
                rhs: Box::new(strip_spans(*rhs)),
                span: dummy,
            },
            Expr::IsNull { expr, negated, .. } => Expr::IsNull {
                expr: Box::new(strip_spans(*expr)),
                negated,
                span: dummy,
            },
            Expr::FunctionCall { name, args, .. } => Expr::FunctionCall {
                name,
                args: args.into_iter().map(strip_spans).collect(),
                span: dummy,
            },
            Expr::Paren { expr, .. } => Expr::Paren {
                expr: Box::new(strip_spans(*expr)),
                span: dummy,
            },
            Expr::Cast { expr, type_name, .. } => Expr::Cast {
                expr: Box::new(strip_spans(*expr)),
                type_name: Ident {
                    name: type_name.name,
                    span: dummy,
                },
                span: dummy,
            },
        }
    }

    fn assert_expr_eq(sql: &str, expected: Expr) {
        assert_eq!(strip_spans(parse_expr_only(sql)), strip_spans(expected));
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        // `1 + 2 * 3`は`1 + (2 * 3)`であって`(1 + 2) * 3`ではない。
        assert_expr_eq(
            "1 + 2 * 3",
            Expr::BinaryOp {
                op: BinaryOperator::Add,
                lhs: int(1),
                rhs: Box::new(Expr::BinaryOp {
                    op: BinaryOperator::Multiply,
                    lhs: int(2),
                    rhs: int(3),
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn unary_minus_binds_tighter_than_addition() {
        // `-1 + 2`は`(-1) + 2`であって`-(1 + 2)`ではない。
        assert_expr_eq(
            "-1 + 2",
            Expr::BinaryOp {
                op: BinaryOperator::Add,
                lhs: Box::new(Expr::UnaryOp {
                    op: UnaryOperator::Negate,
                    expr: int(1),
                    span: Span::new(0, 0),
                }),
                rhs: int(2),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn addition_is_left_associative() {
        // `1 + 2 + 3`は`(1 + 2) + 3`。
        assert_expr_eq(
            "1 + 2 + 3",
            Expr::BinaryOp {
                op: BinaryOperator::Add,
                lhs: Box::new(Expr::BinaryOp {
                    op: BinaryOperator::Add,
                    lhs: int(1),
                    rhs: int(2),
                    span: Span::new(0, 0),
                }),
                rhs: int(3),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parens_override_precedence() {
        // `(1 + 2) * 3`は括弧を優先し、加算が先に評価される木になる。
        assert_expr_eq(
            "(1 + 2) * 3",
            Expr::BinaryOp {
                op: BinaryOperator::Multiply,
                lhs: Box::new(Expr::Paren {
                    expr: Box::new(Expr::BinaryOp {
                        op: BinaryOperator::Add,
                        lhs: int(1),
                        rhs: int(2),
                        span: Span::new(0, 0),
                    }),
                    span: Span::new(0, 0),
                }),
                rhs: int(3),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn not_binds_looser_than_comparison() {
        // `NOT 1 = 2`は`NOT (1 = 2)`であって`(NOT 1) = 2`ではない。
        assert_expr_eq(
            "NOT 1 = 2",
            Expr::UnaryOp {
                op: UnaryOperator::Not,
                expr: Box::new(Expr::BinaryOp {
                    op: BinaryOperator::Eq,
                    lhs: int(1),
                    rhs: int(2),
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn not_binds_tighter_than_and() {
        // `NOT true AND false`は`(NOT true) AND false`であって`NOT (true AND false)`ではない。
        assert_expr_eq(
            "NOT true AND false",
            Expr::BinaryOp {
                op: BinaryOperator::And,
                lhs: Box::new(Expr::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(Expr::BoolLiteral {
                        value: true,
                        span: Span::new(0, 0),
                    }),
                    span: Span::new(0, 0),
                }),
                rhs: Box::new(Expr::BoolLiteral {
                    value: false,
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn and_binds_tighter_than_or() {
        // `true OR false AND false`は`true OR (false AND false)`。
        assert_expr_eq(
            "true OR false AND false",
            Expr::BinaryOp {
                op: BinaryOperator::Or,
                lhs: Box::new(Expr::BoolLiteral {
                    value: true,
                    span: Span::new(0, 0),
                }),
                rhs: Box::new(Expr::BinaryOp {
                    op: BinaryOperator::And,
                    lhs: Box::new(Expr::BoolLiteral {
                        value: false,
                        span: Span::new(0, 0),
                    }),
                    rhs: Box::new(Expr::BoolLiteral {
                        value: false,
                        span: Span::new(0, 0),
                    }),
                    span: Span::new(0, 0),
                }),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_is_null_and_is_not_null() {
        assert_expr_eq(
            "1 IS NULL",
            Expr::IsNull {
                expr: int(1),
                negated: false,
                span: Span::new(0, 0),
            },
        );
        assert_expr_eq(
            "1 IS NOT NULL",
            Expr::IsNull {
                expr: int(1),
                negated: true,
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_function_call_with_multiple_args() {
        assert_expr_eq(
            "f(1, 2)",
            Expr::FunctionCall {
                name: "f".to_string(),
                args: vec![*int(1), *int(2)],
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_function_call_with_no_args() {
        assert_expr_eq(
            "f()",
            Expr::FunctionCall {
                name: "f".to_string(),
                args: vec![],
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_cast() {
        assert_expr_eq(
            "CAST(1 AS TEXT)",
            Expr::Cast {
                expr: int(1),
                type_name: Ident {
                    name: "TEXT".to_string(),
                    span: Span::new(0, 0),
                },
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_column_ref() {
        assert_expr_eq(
            "id",
            Expr::ColumnRef {
                name: "id".to_string(),
                span: Span::new(0, 0),
            },
        );
    }

    #[test]
    fn parses_select_with_multiple_items() {
        let statement = parse_statement("SELECT 1, 2 + 3").unwrap();
        match statement {
            Statement::Select(select) => assert_eq!(select.items.len(), 2),
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_select_with_from_and_where() {
        let statement = parse_statement("SELECT id FROM users WHERE id = 1").unwrap();
        match statement {
            Statement::Select(select) => {
                assert_eq!(select.from.map(|t| t.name), Some("users".to_string()));
                assert!(select.where_clause.is_some());
            }
            other => panic!("SELECT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_create_table() {
        let statement =
            parse_statement("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)").unwrap();
        match statement {
            Statement::CreateTable(create) => {
                assert_eq!(create.table.name, "users");
                assert_eq!(create.columns.len(), 2);
                assert_eq!(create.columns[0].name.name, "id");
                assert_eq!(create.columns[0].type_name.name, "BIGINT");
                assert!(create.columns[0].not_null);
                assert_eq!(create.columns[1].name.name, "name");
                assert!(!create.columns[1].not_null);
            }
            other => panic!("CREATE TABLE文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn parses_insert() {
        let statement = parse_statement("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        match statement {
            Statement::Insert(insert) => {
                assert_eq!(insert.table.name, "users");
                assert_eq!(insert.values.len(), 2);
            }
            other => panic!("INSERT文を期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn accepts_trailing_semicolon() {
        assert!(parse_statement("SELECT 1;").is_ok());
    }

    #[test]
    fn syntax_error_reports_position_of_bad_token() {
        let err = parse_statement("SELECT 1 +").unwrap_err();
        match err {
            DbError::Parse { line, column, .. } => assert_eq!((line, column), (1, 11)),
            other => panic!("DbError::Parseを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn syntax_error_on_missing_select_target() {
        let err = parse_statement("SELECT FROM users").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn syntax_error_on_trailing_garbage() {
        let err = parse_statement("SELECT 1 2").unwrap_err();
        match err {
            DbError::Parse { line, column, .. } => assert_eq!((line, column), (1, 10)),
            other => panic!("DbError::Parseを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn syntax_error_on_unknown_statement_start() {
        let err = parse_statement("DROP TABLE users").unwrap_err();
        assert!(matches!(err, DbError::Parse { .. }));
    }

    #[test]
    fn propagates_lex_error() {
        let err = parse_statement("SELECT 'abc").unwrap_err();
        assert!(matches!(err, DbError::Lex { .. }));
    }
}
