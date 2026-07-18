//! SQLiteとのDifferential Test。
//!
//! 同じ`CREATE TABLE`・`INSERT`・`UPDATE`・`DELETE`をminidbと`rusqlite`
//! (バンドルされたSQLite)の両方に流し、最後に1本の`SELECT`を実行して結果を
//! 突き合わせる。両者のSQL方言はこの章のサブセットの範囲では一致するため、
//! `setup`のSQL文字列はそのまま両方の実行に使い回せる。
//!
//! セル値の表示は、両者の型システムの違いを吸収するために正規化する。
//! `NULL`はどちらも`"NULL"`という文字列に、整数は10進数の文字列表現に、
//! 文字列はそのままの内容にそろえる。`BOOLEAN`は対象外にしている。SQLiteは
//! 真偽値の型を持たず内部的に`0`/`1`の整数として扱うため、`true`/`false`と
//! 表示するminidbとは同じ正規化関数では比較できない。この章で導入する
//! Differential Testは、両者の表示が一致する範囲(`BIGINT`・`TEXT`・`NULL`)に
//! 限定し、`BOOLEAN`列の突き合わせは今後の課題として残す。
//!
//! `ORDER BY`はまだ構文解析器が受理しないため(第21章で追加する)、比較する
//! `SELECT`はどれも`ORDER BY`を持たない。行の順序は、Sequential Scanが
//! 挿入順を保つminidbと、単純な全表走査ではrowid順(たいていは挿入順と一致する)
//! で返すSQLiteの、双方の実装が偶然そろっている前提に乗っている。この前提は
//! 索引やJoinが絡む複雑なクエリでは崩れうるため、`ORDER BY`が使えるように
//! なったら、この節のテストケースにも付け直すべきである。

use minidb::{Database, Value};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

/// `setup`の各文をminidbと`rusqlite`の両方に順に実行してから、`query`を
/// 両方で実行し、正規化した結果の行を突き合わせる。
fn assert_same_result(setup: &[&str], query: &str) {
    let minidb_rows = run_minidb(setup, query);
    let sqlite_rows = run_sqlite(setup, query);
    assert_eq!(
        minidb_rows, sqlite_rows,
        "minidbとSQLiteの結果が一致しません: setup={setup:?}, query={query:?}"
    );
}

fn run_minidb(setup: &[&str], query: &str) -> Vec<Vec<String>> {
    let mut db = Database::memory();
    for statement in setup {
        db.execute(statement)
            .unwrap_or_else(|e| panic!("minidbでの実行に失敗しました: {statement}: {e}"));
    }
    let result = db
        .execute(query)
        .unwrap_or_else(|e| panic!("minidbでの実行に失敗しました: {query}: {e}"));

    result
        .rows()
        .iter()
        .map(|tuple| tuple.values().iter().map(format_minidb_value).collect())
        .collect()
}

fn format_minidb_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Text(s) => s.clone(),
    }
}

fn run_sqlite(setup: &[&str], query: &str) -> Vec<Vec<String>> {
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
                .map(|i| row.get_ref(i).map(format_sqlite_value))
                .collect::<rusqlite::Result<Vec<String>>>()
        })
        .unwrap_or_else(|e| panic!("SQLiteでの実行に失敗しました: {query}: {e}"));

    rows.map(|row| row.expect("行の取得に失敗しました")).collect()
}

fn format_sqlite_value(value: ValueRef) -> String {
    match value {
        ValueRef::Null => "NULL".to_string(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).to_string(),
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
