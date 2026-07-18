//! `SELECT <式>` だけを解釈する仮の構文解析器。
//!
//! 本物の字句解析器と構文解析器は第6章・第7章で実装する。この章の目的は
//! 「SQL文字列を渡すと結果が1行返ってくる」という経路を1本通すことだけなので、
//! ここでの実装は次章以降にまるごと置き換わる前提の仮実装とする。
//! `SELECT`に続く整数リテラル・真偽値リテラル・整数どうしの加算しか読めない。

use crate::error::{DbError, DbResult};
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
                _ => unreachable!(
                    "ToyExpr::Addの両辺はparse_selectが整数リテラルにしか構築しない"
                ),
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
/// これ以外の入力(`SELECT`で始まらない、対象式が空、加算の片方が整数でない等)は
/// すべて`DbError::Parse`を返す。
pub fn parse_select(sql: &str) -> DbResult<ToySelect> {
    let sql = sql.trim();
    let rest = strip_keyword(sql, "SELECT")
        .ok_or_else(|| DbError::Parse(format!("SELECT文ではありません: {sql:?}")))?;
    let rest = rest.trim();
    let rest = rest.strip_suffix(';').unwrap_or(rest).trim();
    if rest.is_empty() {
        return Err(DbError::Parse(
            "SELECTの対象式がありません".to_string(),
        ));
    }

    let expr = parse_expr(rest)?;
    Ok(ToySelect {
        expr_text: rest.to_string(),
        expr,
    })
}

/// 先頭が`keyword`と大文字小文字を無視して一致するとき、それを取り除いた残りを返す。
fn strip_keyword<'a>(sql: &'a str, keyword: &str) -> Option<&'a str> {
    if sql.len() < keyword.len() {
        return None;
    }
    let (head, tail) = sql.split_at(keyword.len());
    head.eq_ignore_ascii_case(keyword).then_some(tail)
}

/// `+`で連結された式を解析する。項が1つなら`parse_term`にそのまま委ねる。
fn parse_expr(src: &str) -> DbResult<ToyExpr> {
    let terms: Vec<&str> = src.split('+').map(str::trim).collect();
    if terms.iter().any(|t| t.is_empty()) {
        return Err(DbError::Parse(format!("式を解析できません: {src:?}")));
    }

    if let [only] = terms.as_slice() {
        return parse_term(only);
    }

    // 2項以上の加算は、このミニ実装では整数リテラルどうしにしか対応しない。
    // 真偽値の加算(`SELECT true + 1;`)のような入力は、ここで弾いて
    // `ToyExpr::Add`の評価側に不正な形が渡らないようにする。
    let mut sum: Option<ToyExpr> = None;
    for term in &terms {
        let n = term
            .parse::<i64>()
            .map_err(|_| DbError::Parse(format!("加算は整数リテラルにのみ対応しています: {term:?}")))?;
        let next = ToyExpr::IntLiteral(n);
        sum = Some(match sum {
            None => next,
            Some(acc) => ToyExpr::Add(Box::new(acc), Box::new(next)),
        });
    }
    Ok(sum.expect("2項以上のtermsを回るループなのでNoneのままにはならない"))
}

/// リテラル1つを解析する。`true`/`false`は真偽値、それ以外は整数として解析する。
fn parse_term(src: &str) -> DbResult<ToyExpr> {
    match src {
        "true" => Ok(ToyExpr::BoolLiteral(true)),
        "false" => Ok(ToyExpr::BoolLiteral(false)),
        _ => src
            .parse::<i64>()
            .map(ToyExpr::IntLiteral)
            .map_err(|_| DbError::Parse(format!("式を解析できません: {src:?}"))),
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
}
