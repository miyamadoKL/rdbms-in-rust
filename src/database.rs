//! SQL文字列を受け取り、結果を返す実行の入口。
//!
//! `Database::execute`は、まずSQL文字列を`parser::parse_statement`でASTへ変換する。
//! 実際に実行できるのは、`FROM`を伴わない`SELECT`の式リストのうち、リテラルと
//! 整数の加算だけである。`CREATE TABLE`・`INSERT`・`FROM`/`WHERE`付きの`SELECT`は
//! 構文解析までは通るが、実行するとカタログや式評価が揃う章(第8〜10章)を指し示す
//! `DbError::NotImplemented`を返す。

use crate::ast::{BinaryOperator, Expr, SelectStatement, Statement};
use crate::error::{DbError, DbResult};
use crate::types::{Column, Schema, Tuple, Value};

/// minidbのデータベース1つを表す。
///
/// 現時点ではインメモリの状態しか持たない。ディスクへの永続化は第2部で
/// `Database::open`のような別のコンストラクタとして追加する。
pub struct Database;

impl Database {
    /// インメモリのDatabaseを作る。
    pub fn memory() -> Self {
        Database
    }

    /// SQL文字列を1本実行し、結果を返す。
    ///
    /// 構文解析(`parser::parse_statement`)がまず走り、`DbError::Lex`または
    /// `DbError::Parse`はそのまま呼び出し元に伝わる。構文解析に成功しても、
    /// この章の時点で実行できない構文(`FROM`/`WHERE`付き`SELECT`、
    /// `CREATE TABLE`、`INSERT`)は`DbError::NotImplemented`を返す。
    pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
        let statement = crate::parser::parse_statement(sql)?;
        match statement {
            Statement::Select(select) => self.execute_select(sql, &select),
            Statement::CreateTable(_) => Err(DbError::NotImplemented(
                "CREATE TABLEの実行(カタログへの登録)は第9章で対応します".to_string(),
            )),
            Statement::Insert(_) => Err(DbError::NotImplemented(
                "INSERTの実行(表への追加)は第10章で対応します".to_string(),
            )),
        }
    }

    fn execute_select(&self, sql: &str, select: &SelectStatement) -> DbResult<QueryResult> {
        if select.from.is_some() || select.where_clause.is_some() {
            return Err(DbError::NotImplemented(
                "FROM・WHEREを伴うSELECTの実行(カタログと表の参照)は第9〜10章で対応します"
                    .to_string(),
            ));
        }

        let mut columns = Vec::with_capacity(select.items.len());
        let mut values = Vec::with_capacity(select.items.len());
        for item in &select.items {
            let value = eval_expr(&item.expr)?;
            let data_type = value.data_type().expect(
                "eval_exprがこの章で返すのはリテラルの評価結果だけであり、NULLにはならない",
            );
            let name = sql[item.span.start..item.span.end].to_string();
            columns.push(Column::new(name, data_type, false));
            values.push(value);
        }

        let schema = Schema::new(columns);
        let tuple = Tuple::new(&schema, values)?;

        Ok(QueryResult {
            schema,
            rows: vec![tuple],
        })
    }
}

/// 式を評価して`Value`を返す。
///
/// この章で評価できるのは、リテラルと整数どうしの加算(`+`)だけである。
/// それ以外の構文(減算・乗除・比較・論理演算・`IS NULL`・列参照・関数呼び出し)は、
/// 構文解析はこの章で完成しているが、評価の意味づけ(型変換、三値論理、NULL伝播)は
/// 第8章の役目なので、ここでは`DbError::NotImplemented`を返す。
fn eval_expr(expr: &Expr) -> DbResult<Value> {
    match expr {
        Expr::IntLiteral { value, .. } => Ok(Value::BigInt(*value)),
        Expr::StringLiteral { value, .. } => Ok(Value::Text(value.clone())),
        Expr::BoolLiteral { value, .. } => Ok(Value::Boolean(*value)),
        Expr::NullLiteral { .. } => Ok(Value::Null),
        Expr::Paren { expr, .. } => eval_expr(expr),
        Expr::BinaryOp {
            op: BinaryOperator::Add,
            lhs,
            rhs,
            ..
        } => match (eval_expr(lhs)?, eval_expr(rhs)?) {
            (Value::BigInt(l), Value::BigInt(r)) => Ok(Value::BigInt(l + r)),
            _ => Err(DbError::NotImplemented(
                "整数以外の加算・型変換は第8章の式評価で対応します".to_string(),
            )),
        },
        _ => Err(DbError::NotImplemented(
            "この式の評価は第8章の式評価で対応します".to_string(),
        )),
    }
}

/// `Database::execute`の結果。列構成(`Schema`)と、それに従う行の並びを持つ。
pub struct QueryResult {
    schema: Schema,
    rows: Vec<Tuple>,
}

impl QueryResult {
    /// 結果の列構成を返す。
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// 結果の行を返す。
    pub fn rows(&self) -> &[Tuple] {
        &self.rows
    }
}

impl std::fmt::Display for QueryResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let header = self
            .schema
            .columns()
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        writeln!(f, "{header}")?;
        writeln!(f, "{}", "-".repeat(header.chars().count().max(1)))?;

        for tuple in &self.rows {
            let row = tuple
                .values()
                .iter()
                .map(format_value)
                .collect::<Vec<_>>()
                .join(" | ");
            writeln!(f, "{row}")?;
        }

        let row_word = if self.rows.len() == 1 { "row" } else { "rows" };
        write!(f, "({} {row_word})", self.rows.len())
    }
}

/// `Value`をユーザー向けの表示形式に変換する。
fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Text(s) => s.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DbError;

    #[test]
    fn executes_integer_literal() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1;").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(1)]);
        assert_eq!(result.schema().columns()[0].name, "1");
    }

    #[test]
    fn executes_addition() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 + 2;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(3)]);
    }

    #[test]
    fn executes_boolean_literal() {
        let mut db = Database::memory();
        let result = db.execute("SELECT true;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Boolean(true)]);
    }

    #[test]
    fn executes_chained_addition_left_to_right() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 + 2 + 3;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(6)]);
    }

    #[test]
    fn multiplication_parses_but_is_not_evaluated_yet() {
        // `parser`は`1 + 2 * 3`を`1 + (2 * 3)`という正しい木に組み立てる
        // (`parser`のテストで確認済み)。ただし乗算の評価自体は第8章の対象なので、
        // `execute`はこの式を解析はできても評価できず`NotImplemented`を返す。
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 + 2 * 3;");
        assert!(matches!(result, Err(DbError::NotImplemented(_))));
    }

    #[test]
    fn executes_multiple_select_items() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1, 2 + 3;").unwrap();
        assert_eq!(
            result.rows()[0].values(),
            &[Value::BigInt(1), Value::BigInt(5)]
        );
        assert_eq!(result.schema().columns()[0].name, "1");
        assert_eq!(result.schema().columns()[1].name, "2 + 3");
    }

    #[test]
    fn propagates_parse_error() {
        let mut db = Database::memory();
        let result = db.execute("this is not sql");
        assert!(matches!(result, Err(DbError::Parse { .. })));
    }

    #[test]
    fn propagates_lex_error_with_position() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 'abc");
        match result {
            Err(DbError::Lex { line, column, .. }) => assert_eq!((line, column), (1, 8)),
            Err(e) => panic!("DbError::Lexを期待したがDbError::Parse等が返った: {e}"),
            Ok(_) => panic!("DbError::Lexを期待したがOkが返った"),
        }
    }

    #[test]
    fn each_execute_call_is_independent() {
        let mut db = Database::memory();
        let first = db.execute("SELECT 1;").unwrap();
        let second = db.execute("SELECT 2;").unwrap();
        assert_eq!(first.rows()[0].values(), &[Value::BigInt(1)]);
        assert_eq!(second.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn display_formats_header_row_and_footer() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1;").unwrap();
        assert_eq!(result.to_string(), "1\n-\n1\n(1 row)");
    }

    #[test]
    fn select_with_from_is_not_implemented_yet() {
        let mut db = Database::memory();
        let result = db.execute("SELECT id FROM users");
        assert!(matches!(result, Err(DbError::NotImplemented(_))));
    }

    #[test]
    fn create_table_is_not_implemented_yet() {
        let mut db = Database::memory();
        let result = db.execute("CREATE TABLE users (id BIGINT NOT NULL)");
        assert!(matches!(result, Err(DbError::NotImplemented(_))));
    }

    #[test]
    fn insert_is_not_implemented_yet() {
        let mut db = Database::memory();
        let result = db.execute("INSERT INTO users VALUES (1)");
        assert!(matches!(result, Err(DbError::NotImplemented(_))));
    }
}
