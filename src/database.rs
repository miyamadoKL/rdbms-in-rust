//! SQL文字列を受け取り、結果を返す実行の入口。
//!
//! `Database::execute`は、まずSQL文字列を`parser::parse_statement`でASTへ変換する。
//! `CREATE TABLE`・`DROP TABLE`は`catalog`モジュールの`Catalog`にテーブル定義を
//! 登録・削除する。`SELECT`・`INSERT`・`UPDATE`・`DELETE`は、`Catalog`でテーブル
//! 定義を引いたうえで、`executor`モジュールのSequential Scan・Filter・
//! Projection・Insert・Update・Delete演算子を正しい順序で呼び出す。行そのものは
//! `storage_mem`モジュールの`MemStorage`が、`TableId`ごとにプロセスのメモリ上へ
//! 保持する。

use crate::ast::{
    CreateTableStatement, DeleteStatement, DropTableStatement, InsertStatement, SelectItem,
    SelectStatement, Statement, UpdateStatement,
};
use crate::catalog::Catalog;
use crate::error::{DbError, DbResult};
use crate::eval::{self, FunctionRegistry};
use crate::executor;
use crate::storage_mem::MemStorage;
use crate::types::{Column, DataType, Schema, Tuple, Value};

/// minidbのデータベース1つを表す。
///
/// Scalar Functionのレジストリ、テーブル定義を保持する`Catalog`、テーブルの行を
/// 保持する`MemStorage`を持つ。テーブルは`Catalog`(定義)と`MemStorage`(中身)の
/// 両方に、同じ`TableId`のもとで存在する。ディスクへの永続化は第2部で
/// `Database::open`のような別のコンストラクタとして追加する。
pub struct Database {
    functions: FunctionRegistry,
    catalog: Catalog,
    storage: MemStorage,
}

impl Database {
    /// インメモリのDatabaseを作る。組み込みのScalar Function(`abs`、`length`)は
    /// 最初から登録済みの状態で始まり、カタログとストレージはどちらも空の
    /// 状態で始まる。
    pub fn memory() -> Self {
        Database {
            functions: FunctionRegistry::with_builtins(),
            catalog: Catalog::new(),
            storage: MemStorage::new(),
        }
    }

    /// 現在のカタログへの参照。
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// SQL文字列を1本実行し、結果を返す。
    ///
    /// 構文解析(`parser::parse_statement`)がまず走り、`DbError::Lex`または
    /// `DbError::Parse`はそのまま呼び出し元に伝わる。
    pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
        let statement = crate::parser::parse_statement(sql)?;
        match statement {
            Statement::Select(select) => self.execute_select(sql, &select),
            Statement::CreateTable(create) => self.execute_create_table(&create),
            Statement::DropTable(drop) => self.execute_drop_table(&drop),
            Statement::Insert(insert) => self.execute_insert(&insert),
            Statement::Update(update) => self.execute_update(&update),
            Statement::Delete(delete) => self.execute_delete(&delete),
        }
    }

    /// `CREATE TABLE`を実行し、列定義を`Schema`へ変換したうえで`Catalog`に登録し、
    /// `MemStorage`に空のテーブルを作る。
    ///
    /// 列名の重複検査(`DbError::DuplicateColumn`)は`Schema::new`自体ではなく、
    /// ここ(`CREATE TABLE`の実行経路)で行う。`Schema`は`SELECT`の出力列を
    /// 表すのにも使われ(`executor::project`)、`SELECT a, a FROM t`のように
    /// 計算結果の列名が重複するのはSQLとして正当なので、`Schema`という型
    /// そのものに「列名は必ず一意」という不変条件を持たせることはできない。
    /// 一意性が必要なのは「実表の列定義」という文脈に限られるため、検査は
    /// その文脈を知っているこの関数に置く。列名の比較は、`Schema::index_of`
    /// や`Catalog`のテーブル名比較と同じく大文字小文字を区別する(`id`と
    /// `ID`は別の列として許す)。
    fn execute_create_table(&mut self, create: &CreateTableStatement) -> DbResult<QueryResult> {
        let mut columns = Vec::with_capacity(create.columns.len());
        let mut seen_names = std::collections::HashSet::with_capacity(create.columns.len());
        for column_def in &create.columns {
            if !seen_names.insert(column_def.name.name.as_str()) {
                return Err(DbError::DuplicateColumn(column_def.name.name.clone()));
            }
            let data_type = DataType::from_sql_name(&column_def.type_name.name).ok_or_else(
                || DbError::Eval(format!("未知の型名です: {}", column_def.type_name.name)),
            )?;
            let nullable = !column_def.not_null;
            columns.push(Column::new(column_def.name.name.clone(), data_type, nullable));
        }

        let schema = Schema::new(columns);
        let id = self.catalog.create_table(&create.table.name, schema)?;
        self.storage.create_table(id);
        Ok(QueryResult::command("CREATE TABLE"))
    }

    /// `DROP TABLE`を実行し、`Catalog`からテーブル定義を、`MemStorage`から
    /// その行をまとめて削除する。
    fn execute_drop_table(&mut self, drop: &DropTableStatement) -> DbResult<QueryResult> {
        let id = self.catalog.drop_table(&drop.table.name)?;
        self.storage.drop_table(id);
        Ok(QueryResult::command("DROP TABLE"))
    }

    fn execute_select(&self, sql: &str, select: &SelectStatement) -> DbResult<QueryResult> {
        match &select.from {
            None => self.execute_select_without_from(sql, select),
            Some(table) => self.execute_select_with_from(sql, select, &table.name),
        }
    }

    /// `FROM`を伴わない`SELECT`。式リストをその場で評価するだけで、行は常に
    /// ちょうど1件返る。
    fn execute_select_without_from(
        &self,
        sql: &str,
        select: &SelectStatement,
    ) -> DbResult<QueryResult> {
        let mut columns = Vec::with_capacity(select.items.len());
        let mut values = Vec::with_capacity(select.items.len());
        for item in &select.items {
            let expr = match item {
                SelectItem::Expr { expr, .. } => expr,
                SelectItem::Wildcard { .. } => {
                    return Err(DbError::Eval(
                        "*はFROMを伴うSELECTでのみ使えます".to_string(),
                    ));
                }
            };
            let value = eval::eval_expr(expr, &self.functions, None)?;
            // `Value::Null`はどの`DataType`にも属さないため、結果列の表示用の型を
            // 決められない。この章ではPostgreSQLの`unknown`型のような専用の型を
            // 別途設けず、`TEXT`をプレースホルダーとして使う(値そのものは
            // `Value::Null`のままなので、表示や後続の計算がこの選択に影響されることはない)。
            let data_type = value.data_type().unwrap_or(DataType::Text);
            let nullable = value.is_null();
            let name = sql[item.span().start..item.span().end].to_string();
            columns.push(Column::new(name, data_type, nullable));
            values.push(value);
        }

        let schema = Schema::new(columns);
        let tuple = Tuple::new(&schema, values)?;

        Ok(QueryResult {
            schema,
            rows: vec![tuple],
            command_tag: None,
        })
    }

    /// `FROM`を伴う`SELECT`。Sequential Scan→(あれば)Filter→Projectionの順に
    /// `executor`の演算子を適用する。
    fn execute_select_with_from(
        &self,
        sql: &str,
        select: &SelectStatement,
        table_name: &str,
    ) -> DbResult<QueryResult> {
        let table_info = self
            .catalog
            .table(table_name)
            .ok_or_else(|| DbError::TableNotFound(table_name.to_string()))?;
        let mem_table = self
            .storage
            .table(table_info.id)
            .expect("catalogに登録されたテーブルはstorageにも必ず存在する");

        let scanned = executor::seq_scan(mem_table);
        let filtered = match &select.where_clause {
            Some(predicate) => {
                executor::filter(&table_info.schema, &self.functions, scanned, predicate)?
            }
            None => scanned,
        };
        let (schema, rows) = executor::project(
            &table_info.schema,
            &self.functions,
            &filtered,
            &select.items,
            sql,
        )?;

        Ok(QueryResult {
            schema,
            rows,
            command_tag: None,
        })
    }

    /// `INSERT INTO`を実行する。`executor::insert`が、`VALUES`の評価から
    /// `MemStorage`への書き込みまでを行う。
    fn execute_insert(&mut self, insert: &InsertStatement) -> DbResult<QueryResult> {
        let table_info = self
            .catalog
            .table(&insert.table.name)
            .ok_or_else(|| DbError::TableNotFound(insert.table.name.clone()))?;
        let schema = &table_info.schema;
        let mem_table = self
            .storage
            .table_mut(table_info.id)
            .expect("catalogに登録されたテーブルはstorageにも必ず存在する");

        let count = executor::insert(
            mem_table,
            schema,
            &self.functions,
            insert.columns.as_deref(),
            &insert.rows,
        )?;
        Ok(QueryResult::command_with_count("INSERT", count))
    }

    /// `UPDATE`を実行する。`executor::update`が、`WHERE`に一致した行への
    /// `SET`の適用までを行う。
    fn execute_update(&mut self, update: &UpdateStatement) -> DbResult<QueryResult> {
        let table_info = self
            .catalog
            .table(&update.table.name)
            .ok_or_else(|| DbError::TableNotFound(update.table.name.clone()))?;
        let schema = &table_info.schema;
        let mem_table = self
            .storage
            .table_mut(table_info.id)
            .expect("catalogに登録されたテーブルはstorageにも必ず存在する");

        let count = executor::update(
            mem_table,
            schema,
            &self.functions,
            &update.assignments,
            update.where_clause.as_ref(),
        )?;
        Ok(QueryResult::command_with_count("UPDATE", count))
    }

    /// `DELETE FROM`を実行する。`executor::delete`が、`WHERE`に一致した行の
    /// 削除までを行う。
    fn execute_delete(&mut self, delete: &DeleteStatement) -> DbResult<QueryResult> {
        let table_info = self
            .catalog
            .table(&delete.table.name)
            .ok_or_else(|| DbError::TableNotFound(delete.table.name.clone()))?;
        let schema = &table_info.schema;
        let mem_table = self
            .storage
            .table_mut(table_info.id)
            .expect("catalogに登録されたテーブルはstorageにも必ず存在する");

        let count = executor::delete(
            mem_table,
            schema,
            &self.functions,
            delete.where_clause.as_ref(),
        )?;
        Ok(QueryResult::command_with_count("DELETE", count))
    }
}

/// `Database::execute`の結果。
///
/// `SELECT`は列構成(`Schema`)と、それに従う行の並びを持つ。`CREATE TABLE`・
/// `DROP TABLE`のようなDDL文と、`INSERT`・`UPDATE`・`DELETE`のようなDML文は
/// 返す行を持たないため、`schema`は空、`rows`も空のベクタになり、代わりに
/// `command_tag`が完了した文の種類を持つ。DDL文は`"CREATE TABLE"`のように
/// 種類の名前だけ、DML文は`"INSERT 2"`のように影響を受けた行数を添えた形式に
/// なる(psqlの`INSERT 0 2`のような追加情報は持たない、この教材の簡略形式)。
/// 行を1件も返さない`SELECT`と区別するためにフィールドを分けている。
pub struct QueryResult {
    schema: Schema,
    rows: Vec<Tuple>,
    command_tag: Option<String>,
}

impl QueryResult {
    /// DDL文が完了したことを表す`QueryResult`を作る。
    fn command(tag: &'static str) -> Self {
        QueryResult {
            schema: Schema::new(Vec::new()),
            rows: Vec::new(),
            command_tag: Some(tag.to_string()),
        }
    }

    /// DML文が完了したことを表す`QueryResult`を作る。`count`は影響を受けた行数。
    fn command_with_count(tag: &'static str, count: usize) -> Self {
        QueryResult {
            schema: Schema::new(Vec::new()),
            rows: Vec::new(),
            command_tag: Some(format!("{tag} {count}")),
        }
    }

    /// 結果の列構成を返す。DDL・DML文の完了では列を持たない空の`Schema`を返す。
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// 結果の行を返す。DDL・DML文の完了では常に空のスライスを返す。
    pub fn rows(&self) -> &[Tuple] {
        &self.rows
    }
}

impl std::fmt::Display for QueryResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(tag) = &self.command_tag {
            return write!(f, "{tag}");
        }

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
    fn multiplication_now_evaluates() {
        // `parser`は`1 + 2 * 3`を`1 + (2 * 3)`という正しい木に組み立てる
        // (`parser`のテストで確認済み)。前章まではこの木の乗算部分を評価できず
        // `NotImplemented`になっていたが、`eval`モジュールが揃ったこの章からは
        // 最後まで評価できる。
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 + 2 * 3;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(7)]);
    }

    #[test]
    fn executes_comparison() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 = 1;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Boolean(true)]);
    }

    #[test]
    fn executes_three_valued_logic() {
        let mut db = Database::memory();
        let result = db.execute("SELECT NULL AND FALSE;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Boolean(false)]);
    }

    #[test]
    fn executes_cast() {
        let mut db = Database::memory();
        let result = db.execute("SELECT CAST(42 AS TEXT);").unwrap();
        assert_eq!(
            result.rows()[0].values(),
            &[Value::Text("42".to_string())]
        );
    }

    #[test]
    fn select_null_literal_is_a_nullable_text_column() {
        // `Value::Null`はどの`DataType`にも属さないため、結果列の型は
        // プレースホルダーとして`TEXT`を選ぶ(`execute_select`のコメント参照)。
        // 値そのものは`Value::Null`のままである。
        let mut db = Database::memory();
        let result = db.execute("SELECT NULL;").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::Null]);
        assert_eq!(result.schema().columns()[0].data_type, DataType::Text);
        assert!(result.schema().columns()[0].nullable);
    }

    #[test]
    fn division_by_zero_is_an_eval_error() {
        let mut db = Database::memory();
        let result = db.execute("SELECT 1 / 0;");
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    #[test]
    fn calls_builtin_function() {
        let mut db = Database::memory();
        let result = db.execute("SELECT abs(-5);").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(5)]);
    }

    #[test]
    fn column_ref_without_from_is_an_eval_error() {
        let mut db = Database::memory();
        let result = db.execute("SELECT id;");
        assert!(matches!(result, Err(DbError::Eval(_))));
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
    fn select_with_from_rejects_unknown_table() {
        let mut db = Database::memory();
        let result = db.execute("SELECT id FROM users");
        assert!(matches!(result, Err(DbError::TableNotFound(name)) if name == "users"));
    }

    #[test]
    fn insert_rejects_unknown_table() {
        let mut db = Database::memory();
        let result = db.execute("INSERT INTO users VALUES (1)");
        assert!(matches!(result, Err(DbError::TableNotFound(name)) if name == "users"));
    }

    // ---- CREATE TABLE ----

    #[test]
    fn create_table_registers_the_table_in_the_catalog() {
        let mut db = Database::memory();
        let result = db
            .execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        assert_eq!(result.to_string(), "CREATE TABLE");

        let info = db.catalog().table("users").unwrap();
        assert_eq!(info.schema.columns().len(), 2);
        assert_eq!(info.schema.columns()[0].name, "id");
        assert_eq!(info.schema.columns()[0].data_type, DataType::BigInt);
        assert!(!info.schema.columns()[0].nullable);
        assert_eq!(info.schema.columns()[1].name, "name");
        assert_eq!(info.schema.columns()[1].data_type, DataType::Text);
        assert!(info.schema.columns()[1].nullable);
    }

    #[test]
    fn create_table_result_has_no_rows() {
        let mut db = Database::memory();
        let result = db
            .execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        assert!(result.rows().is_empty());
        assert!(result.schema().is_empty());
    }

    #[test]
    fn create_table_rejects_duplicate_name() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        let result = db.execute("CREATE TABLE users (id BIGINT NOT NULL)");
        assert!(matches!(result, Err(DbError::DuplicateTable(name)) if name == "users"));
    }

    #[test]
    fn create_table_rejects_unknown_type_name() {
        let mut db = Database::memory();
        let result = db.execute("CREATE TABLE users (id FLOAT)");
        assert!(matches!(result, Err(DbError::Eval(_))));
        // 型名の解決に失敗した時点でカタログには何も登録されない。
        assert!(db.catalog().table("users").is_none());
    }

    #[test]
    fn create_table_rejects_duplicate_column_name() {
        let mut db = Database::memory();
        let result = db.execute("CREATE TABLE dup (id BIGINT, id TEXT)");
        assert!(matches!(result, Err(DbError::DuplicateColumn(name)) if name == "id"));
        // 列名の重複を検出した時点でカタログには何も登録されない。
        assert!(db.catalog().table("dup").is_none());
    }

    #[test]
    fn create_table_column_names_are_case_sensitive() {
        // `id`と`ID`は別列として許す。`Catalog`のテーブル名比較(第9章)と
        // 揃えた方針。
        let mut db = Database::memory();
        let result = db.execute("CREATE TABLE t (id BIGINT, ID TEXT)");
        assert!(result.is_ok());
    }

    // ---- DROP TABLE ----

    #[test]
    fn drop_table_removes_a_registered_table() {
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        let result = db.execute("DROP TABLE users").unwrap();
        assert_eq!(result.to_string(), "DROP TABLE");
        assert!(db.catalog().table("users").is_none());
    }

    #[test]
    fn drop_table_rejects_unknown_table() {
        let mut db = Database::memory();
        let result = db.execute("DROP TABLE users");
        assert!(matches!(result, Err(DbError::TableNotFound(name)) if name == "users"));
    }

    #[test]
    fn create_drop_create_cycle_succeeds() {
        // 削除したテーブル名は再利用できる: 削除→同名で再作成が通ることを確認する。
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
            .unwrap();
        db.execute("DROP TABLE users").unwrap();
        let result = db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)");
        assert!(result.is_ok());
        assert_eq!(db.catalog().table("users").unwrap().schema.columns().len(), 2);
    }

    // ---- INSERT ----

    fn users_db() -> Database {
        let mut db = Database::memory();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        db
    }

    /// `sql`を実行し、`DbError::Eval`のメッセージ文字列を取り出す。それ以外の
    /// 結果(成功、または`Eval`以外のエラー)ならテストを失敗させる。
    /// 空テーブルと非空テーブルでの同じ型エラーの文言を比較するテスト
    /// (`..._is_rejected_with_the_same_error_on_empty_and_non_empty_tables`)が
    /// 共通して使う。
    fn expect_eval_error_message(db: &mut Database, sql: &str) -> String {
        match db.execute(sql) {
            Err(DbError::Eval(message)) => message,
            Ok(_) => panic!("{sql:?}は失敗するはずだったが成功した"),
            Err(other) => panic!("DbError::Evalを期待したが{other}が返った"),
        }
    }

    #[test]
    fn insert_adds_a_row() {
        let mut db = users_db();
        let result = db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        assert_eq!(result.to_string(), "INSERT 1");

        let selected = db.execute("SELECT * FROM users").unwrap();
        assert_eq!(selected.rows().len(), 1);
        assert_eq!(
            selected.rows()[0].values(),
            &[Value::BigInt(1), Value::Text("Alice".to_string())]
        );
    }

    #[test]
    fn insert_accepts_multiple_rows_in_one_statement() {
        let mut db = users_db();
        let result = db
            .execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        assert_eq!(result.to_string(), "INSERT 2");
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);
    }

    #[test]
    fn insert_with_explicit_columns_fills_omitted_columns_with_null() {
        let mut db = users_db();
        db.execute("INSERT INTO users (id) VALUES (1)").unwrap();

        let selected = db.execute("SELECT * FROM users").unwrap();
        assert_eq!(
            selected.rows()[0].values(),
            &[Value::BigInt(1), Value::Null]
        );
    }

    #[test]
    fn insert_with_explicit_columns_in_any_order() {
        let mut db = users_db();
        db.execute("INSERT INTO users (name, id) VALUES ('Alice', 1)")
            .unwrap();

        let selected = db.execute("SELECT * FROM users").unwrap();
        assert_eq!(
            selected.rows()[0].values(),
            &[Value::BigInt(1), Value::Text("Alice".to_string())]
        );
    }

    #[test]
    fn insert_rejects_not_null_violation() {
        let mut db = users_db();
        let result = db.execute("INSERT INTO users (name) VALUES ('Alice')");
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
        // 検査に失敗した行は1件も挿入されない。
        assert!(db.execute("SELECT * FROM users").unwrap().rows().is_empty());
    }

    #[test]
    fn insert_is_all_or_nothing_across_rows() {
        let mut db = users_db();
        // 1行目は妥当だが、2行目が`id`のNOT NULLに違反する。
        let result = db.execute("INSERT INTO users VALUES (1, 'Alice'), (NULL, 'Bob')");
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
        assert!(db.execute("SELECT * FROM users").unwrap().rows().is_empty());
    }

    // ---- SELECT (FROM/WHERE/*) ----

    #[test]
    fn select_star_returns_all_columns_in_schema_order() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let result = db.execute("SELECT * FROM users").unwrap();
        assert_eq!(
            result
                .schema()
                .columns()
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "name"]
        );
    }

    #[test]
    fn select_projects_a_subset_of_columns() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let result = db.execute("SELECT name FROM users").unwrap();
        assert_eq!(result.schema().columns().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::Text("Alice".to_string())]);
    }

    #[test]
    fn select_evaluates_expressions_over_columns() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let result = db.execute("SELECT id + 1 FROM users").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn select_where_filters_rows() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("SELECT id FROM users WHERE id = 2").unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn select_where_drops_unknown_rows() {
        // `name`が`NULL`の行は、`name = 'Alice'`がUNKNOWNになるため落ちる
        // (FALSEになる場合と同じ扱い)。
        let mut db = users_db();
        db.execute("INSERT INTO users (id) VALUES (1)").unwrap();
        db.execute("INSERT INTO users VALUES (2, 'Alice')").unwrap();
        let result = db
            .execute("SELECT id FROM users WHERE name = 'Alice'")
            .unwrap();
        assert_eq!(result.rows().len(), 1);
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn select_from_empty_table_returns_no_rows() {
        let mut db = users_db();
        let result = db.execute("SELECT * FROM users").unwrap();
        assert!(result.rows().is_empty());
        // 行が1件も無くても、列参照の出力Schemaは`table_schema`から正確に決まる。
        assert_eq!(result.schema().columns()[0].data_type, DataType::BigInt);
    }

    #[test]
    fn select_where_1_is_rejected_even_on_an_empty_table() {
        // `users`が空だと`filter`の行ループが1度も回らないため、行を評価して
        // 初めて気づく検査だけでは`WHERE 1`のような書き誤りを見逃してしまう。
        // `check_predicate_type`による事前の静的検査がその穴を塞ぐ。
        let mut db = users_db();
        let result = db.execute("SELECT id FROM users WHERE 1");
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    #[test]
    fn select_where_null_succeeds_with_no_rows() {
        // `WHERE NULL`は型エラーではない。`NULL`はUNKNOWNであり、`TRUE`にならない
        // という理由で正しく「0行」に絞り込まれるべきで、`WHERE 1`のような
        // 型違反とは区別しなければならない。`infer_type`が`NULL`リテラルに対して
        // `None`(型が定まらない)を返し、`check_predicate_type`が`None`を
        // `Some(Boolean)`と同じく許可するのはこのため。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("SELECT id FROM users WHERE NULL").unwrap();
        assert!(result.rows().is_empty());
    }

    #[test]
    fn select_where_1_and_2_is_rejected_with_the_same_error_on_empty_and_non_empty_tables() {
        // `1 AND 2`はトップレベルの演算子(`AND`)だけを見ると`Boolean`を返す形を
        // しているため、`AND`の被演算子の型まで再帰的に検査しないと、空テーブルでは
        // 素通りしてしまう(非空テーブルでは`eval_expr`が行ごとに`1`をBOOLEANとして
        // 扱えず実行時エラーになる、という非対称が生じる)。`infer_type`が`AND`の
        // 両辺を再帰的に検査するようになったことで、空・非空どちらでも同じ文言の
        // エラーになることを確認する。
        let mut empty_db = users_db();
        let empty_message = expect_eval_error_message(&mut empty_db, "SELECT id FROM users WHERE 1 AND 2");

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_message =
            expect_eval_error_message(&mut populated_db, "SELECT id FROM users WHERE 1 AND 2");

        assert_eq!(empty_message, populated_message);
    }

    #[test]
    fn select_where_not_1_is_rejected_with_the_same_error_on_empty_and_non_empty_tables() {
        let mut empty_db = users_db();
        let empty_message = expect_eval_error_message(&mut empty_db, "SELECT id FROM users WHERE NOT 1");

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_message =
            expect_eval_error_message(&mut populated_db, "SELECT id FROM users WHERE NOT 1");

        assert_eq!(empty_message, populated_message);
    }

    #[test]
    fn select_where_abs_of_text_equals_1_is_rejected_with_the_same_error_on_empty_and_non_empty_tables()
     {
        // `abs('x') = 1`は、比較演算子(`=`)自身は正しい形をしていても、`abs`の
        // 引数の型が誤っている。関数の引数型検査(`FunctionRegistry::arg_types`)を
        // `infer_type`から呼ぶことで、この誤りも空・非空どちらのテーブルでも
        // 同じ文言のエラーとして検出できることを確認する。
        let mut empty_db = users_db();
        let empty_message =
            expect_eval_error_message(&mut empty_db, "SELECT id FROM users WHERE abs('x') = 1");

        let mut populated_db = users_db();
        populated_db
            .execute("INSERT INTO users VALUES (1, 'Alice')")
            .unwrap();
        let populated_message = expect_eval_error_message(
            &mut populated_db,
            "SELECT id FROM users WHERE abs('x') = 1",
        );

        assert_eq!(empty_message, populated_message);
    }

    // ---- UPDATE ----

    #[test]
    fn update_changes_matching_rows() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db
            .execute("UPDATE users SET name = 'Carol' WHERE id = 1")
            .unwrap();
        assert_eq!(result.to_string(), "UPDATE 1");

        let selected = db.execute("SELECT id, name FROM users WHERE id = 1").unwrap();
        assert_eq!(
            selected.rows()[0].values(),
            &[Value::BigInt(1), Value::Text("Carol".to_string())]
        );
        let unaffected = db.execute("SELECT name FROM users WHERE id = 2").unwrap();
        assert_eq!(
            unaffected.rows()[0].values(),
            &[Value::Text("Bob".to_string())]
        );
    }

    #[test]
    fn update_without_where_changes_every_row() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("UPDATE users SET name = 'Same'").unwrap();
        assert_eq!(result.to_string(), "UPDATE 2");
    }

    #[test]
    fn update_set_right_hand_side_sees_the_pre_update_row() {
        // `SET id = id + 1, name = name`のような複数代入で、後続の代入が
        // 直前の代入結果を見ないことを確認する(更新前の行を使って評価する)。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        db.execute("UPDATE users SET id = id + 1").unwrap();
        let result = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn update_rejects_not_null_violation() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice')").unwrap();
        let result = db.execute("UPDATE users SET id = NULL");
        assert!(matches!(result, Err(DbError::SchemaMismatch(_))));
        // 検査に失敗したら、対象行は一切書き換わらない。
        let selected = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(selected.rows()[0].values(), &[Value::BigInt(1)]);
    }

    #[test]
    fn update_where_1_is_rejected_even_on_an_empty_table() {
        // `select_where_1_is_rejected_even_on_an_empty_table`と同じ理由で、
        // `users`が空でも`UPDATE ... WHERE 1`は静的検査で拒否される。
        let mut db = users_db();
        let result = db.execute("UPDATE users SET name = 'x' WHERE 1");
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    #[test]
    fn update_where_null_succeeds_with_no_rows_updated() {
        // `select_where_null_succeeds_with_no_rows`と同じ理由で、`WHERE NULL`は
        // 型エラーではなく、単に0行にマッチする。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("UPDATE users SET name = 'x' WHERE NULL").unwrap();
        assert_eq!(result.to_string(), "UPDATE 0");
    }

    // ---- DELETE ----

    #[test]
    fn delete_removes_matching_rows() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("DELETE FROM users WHERE id = 1").unwrap();
        assert_eq!(result.to_string(), "DELETE 1");
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 1);
    }

    #[test]
    fn delete_without_where_removes_every_row() {
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("DELETE FROM users").unwrap();
        assert_eq!(result.to_string(), "DELETE 2");
        assert!(db.execute("SELECT * FROM users").unwrap().rows().is_empty());
    }

    #[test]
    fn delete_where_1_is_rejected_even_on_an_empty_table() {
        // `select_where_1_is_rejected_even_on_an_empty_table`と同じ理由で、
        // `users`が空でも`DELETE ... WHERE 1`は静的検査で拒否される。
        let mut db = users_db();
        let result = db.execute("DELETE FROM users WHERE 1");
        assert!(matches!(result, Err(DbError::Eval(_))));
    }

    #[test]
    fn delete_where_null_succeeds_with_no_rows_deleted() {
        // `select_where_null_succeeds_with_no_rows`と同じ理由で、`WHERE NULL`は
        // 型エラーではなく、単に0行にマッチする。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        let result = db.execute("DELETE FROM users WHERE NULL").unwrap();
        assert_eq!(result.to_string(), "DELETE 0");
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);
    }

    #[test]
    fn insert_select_update_delete_round_trip() {
        // 第1部の到達点を1つのテストとして確認する: INSERT→SELECT→UPDATE→
        // SELECT→DELETE→SELECTが、すべてこの章のコードだけで動く。
        let mut db = users_db();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);

        db.execute("UPDATE users SET name = 'Alicia' WHERE id = 1")
            .unwrap();
        let updated = db.execute("SELECT name FROM users WHERE id = 1").unwrap();
        assert_eq!(
            updated.rows()[0].values(),
            &[Value::Text("Alicia".to_string())]
        );

        db.execute("DELETE FROM users WHERE id = 2").unwrap();
        let remaining = db.execute("SELECT id FROM users").unwrap();
        assert_eq!(remaining.rows().len(), 1);
        assert_eq!(remaining.rows()[0].values(), &[Value::BigInt(1)]);
    }
}
