//! Tuple Lock(第31章、Diskバックエンド)。
//!
//! `tests/interleave.rs`はMemoryバックエンド(ロックの粒度はテーブル単位)で
//! 4つの異常(Lost Update・Dirty Read・Non-repeatable Read・Phantom)が
//! すべて防がれることを確認した。このファイルはDiskバックエンド
//! (`Database::open`、ロックの粒度は`RecordId`単位、`src/database.rs`
//! モジュール冒頭「ロックの粒度」を参照)で同じ4つを確認する。
//!
//! 結果は3対1で分かれる。Lost Update・Dirty Read・Non-repeatable Readは
//! Tuple Lockでも防がれる。**Phantomだけは防がれない。** 新しく挿入される
//! 行の`RecordId`は挿入が終わるまで存在せず、そもそもロックする対象が
//! 無いためである。この残された1つが第32章(Isolation LevelとDeadlock)の
//! 出発点になる。
//!
//! `different_rows_do_not_block_each_other_under_tuple_lock`は、Tuple Lockが
//! Table Lockより細かい理由そのもの(異なる行を書き換える2つの`UPDATE`が
//! 互いにブロックし合わない)を確認する。

mod common;

use common::temp_db_path;
use minidb::error::DbError;
use minidb::{Database, Value};

fn int_value(result: &minidb::QueryResult, row: usize, col: usize) -> i64 {
    match &result.rows()[row].values()[col] {
        Value::BigInt(n) => *n,
        other => panic!("BigIntを期待したが{other:?}が返った"),
    }
}

fn accounts_db(name: &str) -> (Database, std::path::PathBuf) {
    let path = temp_db_path(name);
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    (db, path)
}

/// Lost Update: Tuple Lockでも防がれる。
#[test]
fn lost_update_is_prevented_by_tuple_lock() {
    let (mut db, path) = accounts_db("interleave-disk-lost-update");
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t1_seen = int_value(&read, 0, 0);
    db.execute_in_tx(&t1, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t1_seen + 10)).unwrap();

    // T2の読み取りは、id=1の行にT1が持つExclusiveロックとぶつかりブロックされる。
    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    db.commit_tx(t1).unwrap();

    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t2_seen = int_value(&read, 0, 0);
    assert_eq!(t2_seen, 110);
    db.execute_in_tx(&t2, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t2_seen + 20)).unwrap();
    db.commit_tx(t2).unwrap();

    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&result, 0, 0), 130, "T1の+10とT2の+20がどちらも残っている(Lost Updateが起きない)");
    std::fs::remove_file(&path).unwrap();
}

/// Dirty Read: Tuple Lockでも防がれる。
#[test]
fn dirty_read_is_prevented_by_tuple_lock() {
    let (mut db, path) = accounts_db("interleave-disk-dirty-read");
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    db.rollback_tx(t1).unwrap();

    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let value = int_value(&read, 0, 0);
    db.commit_tx(t2).unwrap();

    assert_eq!(value, 100, "T2は一度もコミットされていない値(70)を読めない(Dirty Readが起きない)");
    std::fs::remove_file(&path).unwrap();
}

/// Non-repeatable Read: Tuple Lockでも防がれる。
#[test]
fn non_repeatable_read_is_prevented_by_tuple_lock() {
    let (mut db, path) = accounts_db("interleave-disk-non-repeatable-read");
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&first, 0, 0), 100);

    assert!(matches!(
        db.execute_in_tx(&t2, "UPDATE accounts SET balance = 70 WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    let second = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let second_value = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_value, 100, "同じT1の中での2回目の読み取りが1回目と同じ値のまま(Non-repeatable Readが起きない)");

    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();
    std::fs::remove_file(&path).unwrap();
}

/// Phantom: Tuple Lockでは防がれない。
///
/// T2の`INSERT`が作る新しい行の`RecordId`は、挿入が終わるまで存在しない。
/// T1が集計のために取ったロックは、その時点で**存在する**行(id=1・id=2)
/// だけを対象にしており、まだ存在しない行を先回りしてロックする手段が無い。
/// そのためT2の`INSERT`はT1の集計中でもブロックされずに成功してしまい、
/// T1の2回目の集計にはT2が挿入した行が(幻として)現れる。
#[test]
fn phantom_read_is_not_prevented_by_tuple_lock() {
    let (mut db, path) = accounts_db("interleave-disk-phantom");
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    assert_eq!(int_value(&first, 0, 0), 2);

    // Table Lock(`tests/interleave.rs`)と違い、T2のINSERTはブロックされない。
    db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)").unwrap();
    db.commit_tx(t2).unwrap();

    let second = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let second_count = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_count, 3, "T2が挿入した行(幻)が2回目の集計に現れている(Phantomはまだ起きる)");
    std::fs::remove_file(&path).unwrap();
}

/// Tuple Lockが Table Lockより細かい理由そのもの: 異なる行を書き換える
/// 2つの`UPDATE`は、Diskバックエンドでは互いにブロックし合わない
/// (Memoryバックエンドの`two_handles_can_be_interleaved_on_one_database`
/// (`src/database.rs`)では、同じ状況がブロックすることと対比してほしい)。
#[test]
fn different_rows_do_not_block_each_other_under_tuple_lock() {
    let (mut db, path) = accounts_db("interleave-disk-different-rows");
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    // t2はt1と別の行(id=2)だけを書き換えるので、ブロックされずに成功する。
    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 55 WHERE id = 2").unwrap();

    db.commit_tx(t1).unwrap();
    db.commit_tx(t2).unwrap();

    let result = db.execute("SELECT id, balance FROM accounts ORDER BY id").unwrap();
    assert_eq!(int_value(&result, 0, 1), 70);
    assert_eq!(int_value(&result, 1, 1), 55);
    std::fs::remove_file(&path).unwrap();
}
