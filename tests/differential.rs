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
//! 結果の比較は、`query`が`ORDER BY`を持つかどうかで2種類を使い分ける
//! (第21章で`ORDER BY`が構文解析器に加わるまでは、常に順序を無視した比較
//! しかできなかった)。
//!
//! `ORDER BY`が無い`SELECT`は、SQLの意味論上、行の順序が未規定である。
//! Sequential Scanが挿入順を保つminidbと、SQLiteの実装詳細(たいてい挿入順や
//! rowid順)がたまたま一致する保証は無いため、単純な`Vec`の順序比較のままだと
//! 両エンジンが正しく同じ行集合を返していても順序差でテストが偽の失敗を
//! することがある。この場合は両側をソートしてから比較する([`assert_same_result`])。
//! ソートしても重複行の個数は保たれるため、`(1,'a'),(1,'a'),(2,'b')`が
//! `(1,'a'),(2,'b'),(1,'a')`とは一致しても`(1,'a'),(2,'b')`とは一致しない、
//! という多重集合としての等価性は失われない。
//!
//! `ORDER BY`がある`SELECT`は、逆に行の順序そのものがSQLの意味論の一部であり、
//! 両エンジンの実装詳細ではなく`ORDER BY`の並べ替えが正しく行の順序を決定
//! していることを確かめたい。この場合はソートせず、順序を保ったまま比較する
//! ([`assert_same_result_ordered`])。`query`文字列に`ORDER BY`という部分文字列
//! (大文字小文字を無視)が含まれるかどうかで、どちらの比較を使うべきかを
//! 呼び出し側に判定させる仕組みは持たず、テストごとに明示的にどちらの関数を
//! 呼ぶかを選ぶ(暗黙の文字列判定は、`'a ORDER BY b'`のような値の中身に
//! たまたま含まれる文字列と、実際の`ORDER BY`句を取り違える余地があるため)。

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

/// [`assert_same_result`]の順序を保つ版(第21章)。`query`が`ORDER BY`を持ち、
/// 行の順序そのものが検証対象であるテストに使う。
fn assert_same_result_ordered(setup: &[&str], query: &str) {
    let (minidb_rows, column_types) = run_minidb(setup, query);
    let sqlite_rows = run_sqlite(setup, query, &column_types);

    assert_eq!(
        minidb_rows, sqlite_rows,
        "minidbとSQLiteの結果(順序込み)が一致しません: setup={setup:?}, query={query:?}"
    );
}

/// `setup`の各文をminidbと`rusqlite`の両方に順に実行してから、
/// `failing_statement`を両方で実行し、どちらもエラーになることだけを
/// 確認する(第20章、`PRIMARY KEY`/`UNIQUE`違反の比較用)。
///
/// エラーの文言そのもの(`assert_same_result`が行毎の値まで突き合わせるのとは
/// 対照的)は比較しない。minidbは`エラー: PRIMARY KEY制約違反です: 列'id'の値1が
/// 重複しています`、SQLiteは`UNIQUE constraint failed: users.id`のように、
/// 両エンジンのエラーメッセージの語彙・形式は最初から一致する設計になっておらず、
/// 実装依存の文字列を比較しても意味のある差分検出にならない。この章で
/// 両エンジンに共通して要求できるのは「制約違反の文は実行されず、エラーとして
/// 拒否される」という意味論だけなので、比較もその1点に絞る。
fn assert_both_error(setup: &[&str], failing_statement: &str) {
    let mut minidb = Database::memory();
    for statement in setup {
        minidb
            .execute(statement)
            .unwrap_or_else(|e| panic!("minidbでの実行に失敗しました: {statement}: {e}"));
    }
    let minidb_result = minidb.execute(failing_statement);
    assert!(
        minidb_result.is_err(),
        "minidbは{failing_statement:?}を制約違反として拒否するはずでした"
    );

    let conn = Connection::open_in_memory().expect("インメモリのSQLite接続を開けません");
    for statement in setup {
        conn.execute(statement, [])
            .unwrap_or_else(|e| panic!("SQLiteでの実行に失敗しました: {statement}: {e}"));
    }
    let sqlite_result = conn.execute(failing_statement, []);
    assert!(
        sqlite_result.is_err(),
        "SQLiteは{failing_statement:?}を制約違反として拒否するはずでした"
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

#[test]
fn primary_key_duplicate_is_rejected_by_both_engines() {
    assert_both_error(
        &[
            "CREATE TABLE users (id BIGINT PRIMARY KEY NOT NULL, name TEXT)",
            "INSERT INTO users VALUES (1, 'Alice')",
        ],
        "INSERT INTO users VALUES (1, 'Bob')",
    );
}

#[test]
fn unique_duplicate_is_rejected_by_both_engines() {
    assert_both_error(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, email TEXT UNIQUE)",
            "INSERT INTO users VALUES (1, 'a@example.com')",
        ],
        "INSERT INTO users VALUES (2, 'a@example.com')",
    );
}

#[test]
fn primary_key_null_is_rejected_by_both_engines_when_declared_not_null() {
    // SQLiteは`INTEGER PRIMARY KEY`(型名が正確に`INTEGER`である場合に限る)を
    // rowidの別名として扱い、その場合だけNULLを「次のrowidを自動採番する」
    // 特別な値として受理する。さらにSQLiteでは、`PRIMARY KEY`単体は標準SQLとは
    // 異なりNOT NULLを含意しない(明示的な`NOT NULL`が無いと非rowidのPRIMARY
    // KEY列にもNULLが入る)。このSQLサブセットの`BIGINT PRIMARY KEY`は
    // rowidの別名にはならないが、SQLite側の「PRIMARY KEYはNOT NULLを含意
    // しない」という緩さは残るため、`NOT NULL`を明示した列で比較し、
    // どちらの実装依存の差異も踏まないようにする。
    assert_both_error(
        &["CREATE TABLE users (id BIGINT PRIMARY KEY NOT NULL, name TEXT)"],
        "INSERT INTO users (name) VALUES ('Alice')",
    );
}

#[test]
fn update_into_a_duplicate_primary_key_is_rejected_by_both_engines() {
    assert_both_error(
        &[
            "CREATE TABLE users (id BIGINT PRIMARY KEY NOT NULL, name TEXT)",
            "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')",
        ],
        "UPDATE users SET id = 1 WHERE id = 2",
    );
}

// ---- 第21章: ORDER BY / LIMIT / OFFSET / DISTINCT / GROUP BY / HAVING / 集約 ----

#[test]
fn order_by_ascending_matches_sqlites_default_null_first_order() {
    // SQLiteの既定の並び順は、`NULL`をどの値よりも小さい値として扱う。
    // `compare_values`(第21章)が採用した全順序は、この既定の挙動に
    // 一致させてある。
    assert_same_result_ordered(
        &["CREATE TABLE t (x BIGINT)", "INSERT INTO t VALUES (3), (1), (NULL), (2)"],
        "SELECT x FROM t ORDER BY x",
    );
}

#[test]
fn order_by_descending_matches_sqlites_null_last_order() {
    assert_same_result_ordered(
        &["CREATE TABLE t (x BIGINT)", "INSERT INTO t VALUES (3), (1), (NULL), (2)"],
        "SELECT x FROM t ORDER BY x DESC",
    );
}

#[test]
fn order_by_multiple_keys_matches_sqlite() {
    assert_same_result_ordered(
        &[
            "CREATE TABLE orders (dept TEXT, amount BIGINT)",
            "INSERT INTO orders VALUES ('eng', 200), ('eng', 100), ('sales', 50)",
        ],
        "SELECT dept, amount FROM orders ORDER BY dept, amount",
    );
}

#[test]
fn order_by_text_uses_byte_order_like_sqlite() {
    assert_same_result_ordered(
        &["CREATE TABLE t (x TEXT)", "INSERT INTO t VALUES ('banana'), ('apple'), ('cherry')"],
        "SELECT x FROM t ORDER BY x",
    );
}

#[test]
fn limit_and_offset_match_sqlite() {
    assert_same_result_ordered(
        &["CREATE TABLE t (x BIGINT)", "INSERT INTO t VALUES (1), (2), (3), (4), (5)"],
        "SELECT x FROM t ORDER BY x LIMIT 2 OFFSET 1",
    );
}

#[test]
fn distinct_matches_sqlite_including_null_deduplication() {
    assert_same_result(
        &["CREATE TABLE t (x BIGINT)", "INSERT INTO t VALUES (1), (1), (NULL), (NULL), (2)"],
        "SELECT DISTINCT x FROM t",
    );
}

#[test]
fn group_by_count_sum_min_max_match_sqlite() {
    assert_same_result(
        &[
            "CREATE TABLE orders (dept TEXT, amount BIGINT)",
            "INSERT INTO orders VALUES ('eng', 100), ('eng', 200), ('sales', 50), ('sales', NULL), ('hr', NULL)",
        ],
        "SELECT dept, COUNT(*), SUM(amount), MIN(amount), MAX(amount) FROM orders GROUP BY dept",
    );
}

#[test]
fn count_star_on_empty_table_matches_sqlite() {
    assert_same_result(&["CREATE TABLE t (x BIGINT)"], "SELECT COUNT(*) FROM t");
}

#[test]
fn having_filters_groups_and_matches_sqlite() {
    assert_same_result(
        &[
            "CREATE TABLE orders (dept TEXT, amount BIGINT)",
            "INSERT INTO orders VALUES ('eng', 100), ('eng', 200), ('sales', 50)",
        ],
        "SELECT dept, COUNT(*) FROM orders GROUP BY dept HAVING COUNT(*) > 1",
    );
}

#[test]
fn group_by_having_order_by_limit_match_sqlite_together() {
    assert_same_result_ordered(
        &[
            "CREATE TABLE orders (dept TEXT, amount BIGINT)",
            "INSERT INTO orders VALUES ('eng', 100), ('eng', 200), ('sales', 50), ('sales', 30), ('hr', 10)",
        ],
        "SELECT dept, SUM(amount) FROM orders GROUP BY dept HAVING COUNT(*) > 1 ORDER BY dept LIMIT 1",
    );
}

#[test]
fn order_by_a_column_outside_the_select_list_matches_sqlite() {
    // `SELECT name FROM t ORDER BY id`という、SELECTの対象式に無い列を
    // ORDER BYで参照する最頻出パターン。PostgreSQL・SQLiteのどちらでも
    // 対応している挙動であり、隠し列(第21章)を経由してminidbでも同じ結果になる。
    assert_same_result_ordered(
        &[
            "CREATE TABLE t (name TEXT, id BIGINT)",
            "INSERT INTO t VALUES ('b', 2), ('a', 1), ('c', 3)",
        ],
        "SELECT name FROM t ORDER BY id",
    );
}

#[test]
fn order_by_an_aggregate_outside_the_select_list_matches_sqlite() {
    assert_same_result_ordered(
        &[
            "CREATE TABLE orders (dept TEXT, amount BIGINT)",
            "INSERT INTO orders VALUES ('eng', 100), ('eng', 200), ('sales', 50)",
        ],
        "SELECT dept FROM orders GROUP BY dept ORDER BY COUNT(*) DESC, dept",
    );
}

// ---- 第22章: JOIN(Nested Loop Join、Hash Join) ----

#[test]
fn inner_equi_join_matches_sqlite() {
    assert_same_result(
        &[
            "CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)",
            "CREATE TABLE orders (customer_id BIGINT, item TEXT)",
            "INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
            "INSERT INTO orders VALUES (1, 'apple'), (1, 'banana'), (2, 'cherry')",
        ],
        "SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id",
    );
}

#[test]
fn inner_join_drops_rows_with_a_null_join_key_just_like_sqlite() {
    // `customer_id`がNULLの注文は、どの顧客の`id`とも一致しようがない
    // (`NULL = NULL`はUNKNOWN)。minidbのHash Join・SQLiteのどちらも、
    // この行を結合結果から自然に落とすはずである。
    assert_same_result(
        &[
            "CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)",
            "CREATE TABLE orders (customer_id BIGINT, item TEXT)",
            "INSERT INTO customers VALUES (1, 'Alice')",
            "INSERT INTO orders VALUES (1, 'apple'), (NULL, 'orphan')",
        ],
        "SELECT customers.name, orders.item FROM customers JOIN orders ON customers.id = orders.customer_id",
    );
}

#[test]
fn inner_join_on_a_non_equality_condition_matches_sqlite() {
    assert_same_result(
        &[
            "CREATE TABLE a (id BIGINT NOT NULL)",
            "CREATE TABLE b (id BIGINT NOT NULL)",
            "INSERT INTO a VALUES (1), (2), (3)",
            "INSERT INTO b VALUES (1), (2)",
        ],
        "SELECT a.id, b.id FROM a JOIN b ON a.id > b.id",
    );
}

#[test]
fn three_way_join_matches_sqlite() {
    assert_same_result(
        &[
            "CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)",
            "CREATE TABLE orders (customer_id BIGINT, item TEXT)",
            "CREATE TABLE shippers (item TEXT, carrier TEXT)",
            "INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob')",
            "INSERT INTO orders VALUES (1, 'apple'), (2, 'banana')",
            "INSERT INTO shippers VALUES ('apple', 'FastCo'), ('banana', 'SlowCo')",
        ],
        "SELECT customers.name, shippers.carrier FROM customers \
         JOIN orders ON customers.id = orders.customer_id \
         JOIN shippers ON orders.item = shippers.item",
    );
}

#[test]
fn join_with_where_and_group_by_matches_sqlite() {
    assert_same_result(
        &[
            "CREATE TABLE customers (id BIGINT NOT NULL, name TEXT)",
            "CREATE TABLE orders (customer_id BIGINT, item TEXT)",
            "INSERT INTO customers VALUES (1, 'Alice'), (2, 'Bob')",
            "INSERT INTO orders VALUES (1, 'apple'), (1, 'banana'), (2, 'cherry')",
        ],
        "SELECT customers.name, COUNT(*) FROM customers JOIN orders ON customers.id = orders.customer_id \
         WHERE orders.item <> 'banana' GROUP BY customers.name",
    );
}

#[test]
fn duplicate_join_keys_produce_a_cross_product_of_matching_rows_like_sqlite() {
    // 両側に同じ結合キーの行が複数あるとき、結果はその組み合わせの数だけ
    // 増える(2件×2件=4件)。Hash Joinの`build`が鍵ごとに複数行を保持できて
    // いることを、SQLiteとの突き合わせで確認する。
    assert_same_result(
        &[
            "CREATE TABLE a (id BIGINT NOT NULL, label TEXT)",
            "CREATE TABLE b (id BIGINT NOT NULL, note TEXT)",
            "INSERT INTO a VALUES (1, 'a1'), (1, 'a2')",
            "INSERT INTO b VALUES (1, 'b1'), (1, 'b2')",
        ],
        "SELECT a.label, b.note FROM a JOIN b ON a.id = b.id",
    );
}
