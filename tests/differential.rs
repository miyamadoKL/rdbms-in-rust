//! SQLiteとのDifferential Test。
//!
//! 同じ`CREATE TABLE`・`INSERT`・`UPDATE`・`DELETE`をminidbと`rusqlite`
//! (バンドルされたSQLite)の両方に流し、最後に1本の`SELECT`を実行して結果を
//! 突き合わせる。両者のSQL方言はこの章のサブセットの範囲では一致するため、
//! `setup`のSQL文字列はそのまま両方の実行に使い回せる。
//!
//! セル値は、両者の型システムの違いを吸収するために[`DiffValue`]という型タグ
//! 付きの中間表現へ変換してから比較する。`.to_string()`のような文字列化を
//! 経由すると、`NULL`と文字列`'NULL'`、整数`1`と文字列`'1'`が同じ表現に潰れて
//! しまい、区別できなくなる。`DiffValue`はNULL・整数・文字列・真偽値を別の
//! variantとして持つため、この種の型の違いを保ったまま比較できる。
//!
//! `BOOLEAN`列の扱い: SQLiteは真偽値の型を持たず、内部的に`0`/`1`の整数として
//! 扱う。値だけを見て「0または1の整数だから真偽値だろう」と推測するのは、
//! 本物の`BIGINT`列にたまたま`0`や`1`が入っているケースと区別できず危険なので
//! やらない。代わりに、`setup`の`CREATE TABLE`をminidb側で実行した結果
//! (`QueryResult::schema`)から、SELECTした各列の`DataType`が分かる。この
//! スキーマ情報を使って「`DataType::Boolean`と分かっている列でだけ、SQLiteの
//! 整数`0`/`1`を`false`/`true`として読み替える」という列型ベースの変換を行う。
//!
//! 結果の比較は行の順序を無視した多重集合(bag)比較にしている。`ORDER BY`は
//! まだ構文解析器が受理しないため(第21章で追加する)、この章で比較する
//! `SELECT`はどれも`ORDER BY`を持たない。SQLの意味論上、`ORDER BY`の無い
//! `SELECT`の行順序は未規定であり、Sequential Scanが挿入順を保つminidbと、
//! SQLiteの実装詳細(たいてい挿入順やrowid順)がたまたま一致する保証は無い。
//! 単純な`Vec`の順序比較のままだと、両エンジンが正しく同じ行集合を返して
//! いても順序差でテストが偽の失敗をすることがあるため、両側をソートしてから
//! 比較する。ソートしても重複行の個数は保たれるため、`(1,'a'),(1,'a'),(2,'b')`
//! が`(1,'a'),(2,'b'),(1,'a')`とは一致しても`(1,'a'),(2,'b')`とは一致しない、
//! という多重集合としての等価性は失われない。

use minidb::{DataType, Database, Value};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

/// 両エンジンの結果セルを型タグ付きで比較するための中間表現。
///
/// `.to_string()`のような文字列化を経由しないことで、`Null`と`Text("NULL")`、
/// `Integer(1)`と`Text("1")`のような異なる型の値を取り違えずに比較できる。
/// `Ord`を導出しているのは、Differential
/// Testの行比較を「両側をソートしてから比較する」多重集合比較にするため。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DiffValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Text(String),
}

/// `setup`の各文をminidbと`rusqlite`の両方に順に実行してから、`query`を
/// 両方で実行し、型タグ付きの結果行を突き合わせる。
///
/// `SELECT`に`ORDER BY`が無い場合、行の順序はどちらのエンジンでも未規定
/// なので、比較の前に両側をそれぞれソートし、行の集合としての一致(重複行の
/// 個数まで含めた多重集合としての一致)だけを見る。
fn assert_same_result(setup: &[&str], query: &str) {
    let (mut minidb_rows, column_types) = run_minidb(setup, query);
    let mut sqlite_rows = run_sqlite(setup, query, &column_types);

    minidb_rows.sort();
    sqlite_rows.sort();

    assert_eq!(
        minidb_rows, sqlite_rows,
        "minidbとSQLiteの結果が一致しません: setup={setup:?}, query={query:?}"
    );
}

/// minidbで`setup`・`query`を実行し、結果行を`DiffValue`へ変換して返す。
/// あわせて、SQLite側の`0`/`1`をBOOLEANとして読み替えるために使う、
/// `query`が返す各列の`DataType`も返す。
fn run_minidb(setup: &[&str], query: &str) -> (Vec<Vec<DiffValue>>, Vec<DataType>) {
    let mut db = Database::memory();
    for statement in setup {
        db.execute(statement)
            .unwrap_or_else(|e| panic!("minidbでの実行に失敗しました: {statement}: {e}"));
    }
    let result = db
        .execute(query)
        .unwrap_or_else(|e| panic!("minidbでの実行に失敗しました: {query}: {e}"));

    let column_types = result
        .schema()
        .columns()
        .iter()
        .map(|c| c.data_type)
        .collect();

    let rows = result
        .rows()
        .iter()
        .map(|tuple| tuple.values().iter().map(to_diff_value).collect())
        .collect();

    (rows, column_types)
}

fn to_diff_value(value: &Value) -> DiffValue {
    match value {
        Value::Null => DiffValue::Null,
        Value::Boolean(b) => DiffValue::Boolean(*b),
        Value::BigInt(n) => DiffValue::Integer(*n),
        Value::Text(s) => DiffValue::Text(s.clone()),
    }
}

/// SQLiteで`setup`・`query`を実行し、結果行を`DiffValue`へ変換して返す。
///
/// `column_types`はminidb側で`query`を実行して得た列ごとの`DataType`。
/// `column_types[i]`が`DataType::Boolean`である列については、SQLiteが返す
/// 整数`0`/`1`を`false`/`true`として読み替える。
fn run_sqlite(setup: &[&str], query: &str, column_types: &[DataType]) -> Vec<Vec<DiffValue>> {
    let conn = Connection::open_in_memory().expect("インメモリのSQLite接続を開けません");
    for statement in setup {
        conn.execute(statement, [])
            .unwrap_or_else(|e| panic!("SQLiteでの実行に失敗しました: {statement}: {e}"));
    }

    let mut stmt = conn
        .prepare(query)
        .unwrap_or_else(|e| panic!("SQLiteでのprepareに失敗しました: {query}: {e}"));
    let column_count = stmt.column_count();

    let rows = stmt
        .query_map([], |row| {
            (0..column_count)
                .map(|i| {
                    row.get_ref(i)
                        .map(|v| to_diff_value_sqlite(v, column_types.get(i).copied()))
                })
                .collect::<rusqlite::Result<Vec<DiffValue>>>()
        })
        .unwrap_or_else(|e| panic!("SQLiteでの実行に失敗しました: {query}: {e}"));

    rows.map(|row| row.expect("行の取得に失敗しました")).collect()
}

/// SQLiteの`ValueRef`を`DiffValue`へ変換する。
///
/// `column_type`が`Some(DataType::Boolean)`のとき、SQLiteの整数`0`/`1`を
/// `false`/`true`として読み替える。それ以外の列型(または列型が不明)では
/// 整数はそのまま`DiffValue::Integer`にする。
fn to_diff_value_sqlite(value: ValueRef, column_type: Option<DataType>) -> DiffValue {
    match value {
        ValueRef::Null => DiffValue::Null,
        ValueRef::Integer(n) => {
            if column_type == Some(DataType::Boolean) {
                DiffValue::Boolean(n != 0)
            } else {
                DiffValue::Integer(n)
            }
        }
        ValueRef::Real(f) => {
            // このサブセットにREAL型の列は無いはずだが、`.to_string()`相当の
            // 表現をそのまま残しておく(以前の実装からの後方互換で、実際に
            // このアームを踏むテストケースは無い想定)。
            DiffValue::Text(f.to_string())
        }
        ValueRef::Text(bytes) => DiffValue::Text(String::from_utf8_lossy(bytes).to_string()),
        ValueRef::Blob(_) => panic!("このサブセットにBLOBは存在しないはずです"),
    }
}

#[test]
fn create_insert_select() {
    assert_same_result(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
            "INSERT INTO users VALUES (1, 'Alice')",
        ],
        "SELECT id, name FROM users",
    );
}

#[test]
fn multi_row_insert_with_where() {
    assert_same_result(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
            "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
        ],
        "SELECT id, name FROM users WHERE id > 1",
    );
}

#[test]
fn insert_with_explicit_columns_fills_omitted_columns_with_null() {
    assert_same_result(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
            "INSERT INTO users (id) VALUES (1)",
            "INSERT INTO users (id, name) VALUES (2, 'Bob')",
        ],
        "SELECT id, name FROM users",
    );
}

#[test]
fn where_with_null_drops_unknown_rows() {
    // `name = 'Alice'`は、`name`が`NULL`の行ではUNKNOWNになり、FALSEの行と
    // 同じく結果から落ちる。この三値論理の扱いがminidbとSQLiteで一致することを
    // 確認する。
    assert_same_result(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
            "INSERT INTO users (id) VALUES (1)",
            "INSERT INTO users VALUES (2, 'Alice')",
        ],
        "SELECT id FROM users WHERE name = 'Alice'",
    );
}

#[test]
fn update_changes_matching_rows() {
    assert_same_result(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
            "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')",
            "UPDATE users SET name = 'Carol' WHERE id = 1",
        ],
        "SELECT id, name FROM users",
    );
}

#[test]
fn delete_removes_matching_rows() {
    assert_same_result(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
            "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
            "DELETE FROM users WHERE id = 2",
        ],
        "SELECT id, name FROM users",
    );
}

#[test]
fn arithmetic_and_string_comparison_in_where() {
    assert_same_result(
        &[
            "CREATE TABLE accounts (id BIGINT NOT NULL, balance BIGINT NOT NULL)",
            "INSERT INTO accounts VALUES (1, 100), (2, 250), (3, 300)",
        ],
        "SELECT id, balance + 10 FROM accounts WHERE balance > 100",
    );
}

#[test]
fn multi_row_insert_returns_rows_in_any_order() {
    // `ORDER BY`が無いため行順序は未規定。minidbは挿入順、SQLiteはrowid順で
    // 返すことが多く偶然一致しがちだが、`assert_same_result`はソートしてから
    // 比較するため、たとえ内部実装の走査順が食い違っても偽の失敗にならない
    // ことをこのテストで確認する。
    assert_same_result(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
            "INSERT INTO users VALUES (3, 'Carol'), (1, 'Alice'), (2, 'Bob')",
        ],
        "SELECT id, name FROM users",
    );
}

#[test]
fn bag_comparison_preserves_duplicate_row_counts() {
    // 多重集合比較はソートするだけで、重複行の個数を潰さない
    // (`(1,'a'),(1,'a'),(2,'b')`は`(1,'a'),(2,'b')`とは一致しない)ことを
    // minidbとSQLiteの両方で同じ重複行が返る場合を通して確認する。
    assert_same_result(
        &[
            "CREATE TABLE tags (id BIGINT NOT NULL, label TEXT)",
            "INSERT INTO tags VALUES (1, 'a'), (1, 'a'), (2, 'b')",
        ],
        "SELECT id, label FROM tags",
    );
}

#[test]
fn null_is_not_confused_with_the_text_null() {
    // `Value::Null`と文字列`"NULL"`はどちらも素朴な`.to_string()`変換では
    // `"NULL"`という同じ文字列になってしまう。`DiffValue`はvariantを分けている
    // ため、この2つを取り違えずに区別できることを確認する。
    assert_same_result(
        &[
            "CREATE TABLE t (id BIGINT NOT NULL, label TEXT)",
            "INSERT INTO t (id) VALUES (1)",
            "INSERT INTO t VALUES (2, 'NULL')",
        ],
        "SELECT id, label FROM t",
    );
}

#[test]
fn integer_is_not_confused_with_its_text_representation() {
    // 整数`1`と文字列`'1'`はどちらも`.to_string()`変換では`"1"`という同じ
    // 文字列になってしまう。`DiffValue::Integer`と`DiffValue::Text`が別
    // variantであることを確認する。
    assert_same_result(
        &[
            "CREATE TABLE t (n BIGINT NOT NULL, s TEXT NOT NULL)",
            "INSERT INTO t VALUES (1, '1')",
        ],
        "SELECT n, s FROM t",
    );
}

#[test]
fn boolean_column_is_reconciled_against_sqlite_zero_one() {
    // SQLiteには真偽値の型が無く、`BOOLEAN`列は内部的に整数`0`/`1`として
    // 保持される。minidb側の`query`のスキーマ(`DataType::Boolean`)を根拠に
    // SQLiteの`0`/`1`を`false`/`true`として読み替えられることを確認する。
    assert_same_result(
        &[
            "CREATE TABLE flags (id BIGINT NOT NULL, active BOOLEAN NOT NULL)",
            "INSERT INTO flags VALUES (1, TRUE), (2, FALSE)",
        ],
        "SELECT id, active FROM flags",
    );
}
