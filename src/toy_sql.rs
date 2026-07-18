//! `SELECT <式>` だけを解釈する仮の構文解析器。
//!
//! 本物の構文解析器は第7章で実装する。この章の目的は「SQL文字列を渡すと結果が
//! 1行返ってくる」という経路を1本通すことだけなので、ここでの実装は次章以降に
//! まるごと置き換わる前提の仮実装とする。字句解析だけは第6章の`lexer`に委ね、
//! `SELECT`に続く整数リテラル・真偽値リテラル・整数どうしの加算しか読めない。

use crate::error::{DbError, DbResult};
use crate::lexer::{self, Keyword, Token, TokenKind};
use crate::types::Value;

/// この仮実装が扱える式。整数リテラル・真偽値リテラル・整数の加算のみを持つ。
#[derive(Debug, Clone, PartialEq)]
pub enum ToyExpr {
    /// 整数リテラル。
    IntLiteral(i64),
    /// 真偽値リテラル。
    BoolLiteral(bool),
    /// 整数どうしの加算。両辺は整数を返す式であることを構文解析側が保証する。
    Add(Box<ToyExpr>, Box<ToyExpr>),
}

impl ToyExpr {
    /// 式を評価して`Value`を返す。
    pub fn eval(&self) -> Value {
        match self {
            ToyExpr::IntLiteral(v) => Value::BigInt(*v),
            ToyExpr::BoolLiteral(v) => Value::Boolean(*v),
            ToyExpr::Add(lhs, rhs) => match (lhs.eval(), rhs.eval()) {
                (Value::BigInt(l), Value::BigInt(r)) => Value::BigInt(l + r),
                _ => unreachable!("ToyExpr::Addの両辺はparse_selectが整数リテラルにしか構築しない"),
            },
        }
    }
}

/// `parse_select`が返す、`SELECT`文1本の解析結果。
pub struct ToySelect {
    /// `SELECT`と`;`を取り除いた、対象式の元のテキスト。結果の列名に使う。
    pub expr_text: String,
    /// 対象式を解析した`ToyExpr`。
    pub expr: ToyExpr,
}

/// `SELECT <式> [;]`を解析する。
///
/// 対応する構文は次のみ。
/// - 整数リテラル: `1`、`42`
/// - 真偽値リテラル: `true`、`false`
/// - 整数どうしの加算: `1 + 2`
///
/// 字句解析(`lexer::tokenize`)が失敗した場合は`DbError::Lex`を、トークン列は
/// 得られたがこの仮実装が読める構文でない場合(`SELECT`で始まらない、対象式が
/// 空、加算の片方が整数でない等)は`DbError::Parse`を返す。
pub fn parse_select(sql: &str) -> DbResult<ToySelect> {
    let sql = sql.trim();
    let tokens = lexer::tokenize(sql)?;
    let mut iter = tokens.iter();

    match iter.next().map(|t| &t.kind) {
        Some(TokenKind::Keyword(Keyword::Select)) => {}
        _ => return Err(DbError::Parse(format!("SELECT文ではありません: {sql:?}"))),
    }

    let rest: Vec<&Token> = iter
        .take_while(|t| !matches!(t.kind, TokenKind::Semicolon | TokenKind::Eof))
        .collect();

    if rest.is_empty() {
        return Err(DbError::Parse("SELECTの対象式がありません".to_string()));
    }

    let expr = parse_expr(&rest)?;
    Ok(ToySelect {
        expr_text: expr_text(sql, &rest),
        expr,
    })
}

/// 対象式のトークン列が、元のSQL文字列中で占める範囲をそのまま切り出す。
///
/// トークンの`Span`を経由することで、`SELECT 1 + 2;`から`"1 + 2"`のように、
/// 内部の空白は保ちつつ前後の`SELECT`と`;`だけを取り除いたテキストが得られる。
fn expr_text(sql: &str, tokens: &[&Token]) -> String {
    let start = tokens
        .first()
        .expect("空でないことを呼び出し元(parse_select)が保証する")
        .span
        .start;
    let end = tokens
        .last()
        .expect("空でないことを呼び出し元(parse_select)が保証する")
        .span
        .end;
    sql[start..end].to_string()
}

/// `+`で連結された式を解析する。項が1つなら`parse_term`にそのまま委ねる。
fn parse_expr(tokens: &[&Token]) -> DbResult<ToyExpr> {
    let terms: Vec<&[&Token]> = tokens
        .split(|t| matches!(t.kind, TokenKind::Plus))
        .collect();
    if terms.iter().any(|t| t.is_empty()) {
        return Err(DbError::Parse("式を解析できません".to_string()));
    }

    if let [only] = terms.as_slice() {
        return parse_term(only);
    }

    // 2項以上の加算は、このミニ実装では整数リテラルどうしにしか対応しない。
    // 真偽値の加算(`SELECT true + 1;`)のような入力は、ここで弾いて
    // `ToyExpr::Add`の評価側に不正な形が渡らないようにする。
    let mut sum: Option<ToyExpr> = None;
    for term in terms {
        let n = match term {
            [t] => match &t.kind {
                TokenKind::IntLiteral(n) => *n,
                _ => {
                    return Err(DbError::Parse(
                        "加算は整数リテラルにのみ対応しています".to_string(),
                    ));
                }
            },
            _ => {
                return Err(DbError::Parse(
                    "加算は整数リテラルにのみ対応しています".to_string(),
                ));
            }
        };
        let next = ToyExpr::IntLiteral(n);
        sum = Some(match sum {
            None => next,
            Some(acc) => ToyExpr::Add(Box::new(acc), Box::new(next)),
        });
    }
    Ok(sum.expect("2項以上のtermsを回るループなのでNoneのままにはならない"))
}

/// トークン1個分のリテラルを解析する。`true`/`false`は真偽値、整数リテラルは整数。
fn parse_term(tokens: &[&Token]) -> DbResult<ToyExpr> {
    match tokens {
        [t] => match &t.kind {
            TokenKind::IntLiteral(n) => Ok(ToyExpr::IntLiteral(*n)),
            TokenKind::Keyword(Keyword::True) => Ok(ToyExpr::BoolLiteral(true)),
            TokenKind::Keyword(Keyword::False) => Ok(ToyExpr::BoolLiteral(false)),
            _ => Err(DbError::Parse("式を解析できません".to_string())),
        },
        _ => Err(DbError::Parse("式を解析できません".to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_integer_literal() {
        let select = parse_select("SELECT 1;").unwrap();
        assert_eq!(select.expr, ToyExpr::IntLiteral(1));
        assert_eq!(select.expr_text, "1");
    }

    #[test]
    fn parses_addition_of_integers() {
        let select = parse_select("SELECT 1 + 2;").unwrap();
        assert_eq!(select.expr.eval(), Value::BigInt(3));
        assert_eq!(select.expr_text, "1 + 2");
    }

    #[test]
    fn parses_boolean_literal() {
        let select = parse_select("SELECT true;").unwrap();
        assert_eq!(select.expr, ToyExpr::BoolLiteral(true));
    }

    #[test]
    fn accepts_select_without_trailing_semicolon() {
        let select = parse_select("SELECT 1").unwrap();
        assert_eq!(select.expr, ToyExpr::IntLiteral(1));
    }

    #[test]
    fn keyword_match_is_case_insensitive() {
        let select = parse_select("select 1;").unwrap();
        assert_eq!(select.expr, ToyExpr::IntLiteral(1));
    }

    #[test]
    fn rejects_input_without_select_keyword() {
        let result = parse_select("1;");
        assert!(matches!(result, Err(DbError::Parse(_))));
    }

    #[test]
    fn rejects_empty_target_expression() {
        let result = parse_select("SELECT ;");
        assert!(matches!(result, Err(DbError::Parse(_))));
    }

    #[test]
    fn rejects_addition_with_non_integer_operand() {
        let result = parse_select("SELECT true + 1;");
        assert!(matches!(result, Err(DbError::Parse(_))));
    }

    #[test]
    fn rejects_unparseable_expression() {
        let result = parse_select("SELECT abc;");
        assert!(matches!(result, Err(DbError::Parse(_))));
    }

    #[test]
    fn propagates_lex_error_for_unterminated_string_literal() {
        let result = parse_select("SELECT 'abc");
        match result {
            Err(DbError::Lex { line, column, .. }) => assert_eq!((line, column), (1, 8)),
            Err(e) => panic!("DbError::Lexを期待したがDbError::Parse等が返った: {e}"),
            Ok(_) => panic!("DbError::Lexを期待したがOkが返った"),
        }
    }
}
