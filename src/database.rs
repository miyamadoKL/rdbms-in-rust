//! SQL文字列を受け取り、結果を返す最初の縦切り。
//!
//! `Database::execute`は、この章時点では`toy_sql`の仮パーサに解析を委ね、
//! 得られた式を評価して1行だけの`QueryResult`を返す。

use crate::error::DbResult;
use crate::toy_sql;
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
    /// 現時点で受理できる構文は`SELECT <式>;`のみ。構文の解析は`toy_sql`の
    /// 仮実装に委ねている。
    pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
        let select = toy_sql::parse_select(sql)?;
        let value = select.expr.eval();
        let data_type = value
            .data_type()
            .expect("toy_sqlが生成する式はリテラルの評価結果しか返さず、NULLにはならない");

        let schema = Schema::new(vec![Column::new(select.expr_text, data_type, false)]);
        let tuple = Tuple::new(&schema, vec![value])?;

        Ok(QueryResult {
            schema,
            rows: vec![tuple],
        })
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
    fn propagates_parse_error() {
        let mut db = Database::memory();
        let result = db.execute("this is not sql");
        assert!(matches!(result, Err(DbError::Parse(_))));
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
}
