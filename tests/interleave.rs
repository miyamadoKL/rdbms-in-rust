//! 決定的インターリーブテストハーネス(第30章)+ Table Lock(第31章、Memory
//! バックエンド)。
//!
//! `Database::begin_tx`・`execute_in_tx`・`commit_tx`・`rollback_tx`は、通常の
//! SQL経路(`db.execute("BEGIN")`)とは別の、複数のトランザクションを同時に
//! 開けるハーネス専用のAPIである(詳しくは`src/database.rs`の「決定的
//! インターリーブテストハーネス専用の内部API」を参照)。
//!
//! 第30章時点のこのファイルは、minidbがまだ並行制御を持たないことを前提に、
//! Lost Update・Dirty Read・Non-repeatable Read・Phantomという4つの異常が
//! 実際に「起きる」ことを`assert`していた。この章(第31章)からは、`SELECT`・
//! `INSERT`・`UPDATE`・`DELETE`がShared/Exclusiveロックを獲得してから実行
//! するようになったため、この4つの`assert`を反転させる。
//!
//! このファイルが使う`temp_db()`はMemoryバックエンドであり、ロックの粒度は
//! テーブル単位である(`RecordId`という概念を持たないため、`crate::database`
//! モジュール冒頭「ロックの粒度」の説明のとおり)。テーブル単位のロックは
//! 粗いが、正しさは疑いようがなく、この章の4つのテストが示すとおり4つの
//! 異常すべてを防ぐ。より細かい粒度(Tuple Lock、`RecordId`単位)は
//! `Backend::Disk`だけが持ち、Phantomを防げないまま残す
//! (`tests/interleave_disk.rs`を参照)。

mod common;

use common::temp_db;
use minidb::error::DbError;
use minidb::Value;

fn int_value(result: &minidb::QueryResult, row: usize, col: usize) -> i64 {
    match &result.rows()[row].values()[col] {
        Value::BigInt(n) => *n,
        other => panic!("BigIntを期待したが{other:?}が返った"),
    }
}

/// Lost Update: Table Lockのもとでは起きない。
///
/// T1が読んで書いている間、T2の`SELECT`はT1のExclusiveロックとぶつかり
/// ブロックされる。T1がコミットしてロックを手放して初めてT2は読めるため、
/// T2が読む値はT1の書き込みより前の値(古い100)ではなく、T1が確定させた
/// あとの値(110)になる。その結果、T2が計算する新しい値はT1の更新を
/// 上書きせず、両方の更新が残る。
#[test]
fn lost_update_is_prevented_by_table_lock() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t1_seen = int_value(&read, 0, 0);
    assert_eq!(t1_seen, 100);
    db.execute_in_tx(&t1, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t1_seen + 10)).unwrap();

    // T2の読み取りは、T1がaccountsのExclusiveロックを持っている間ブロックされる。
    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    db.commit_tx(t1).unwrap();

    // T1がコミットしてロックを手放したので、T2は同じ文を再試行して読める。
    // T2が読むのはT1が確定させたあとの値(110)であり、古い100ではない。
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t2_seen = int_value(&read, 0, 0);
    assert_eq!(t2_seen, 110);
    db.execute_in_tx(&t2, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t2_seen + 20)).unwrap();
    db.commit_tx(t2).unwrap();

    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&result, 0, 0), 130, "T1の+10とT2の+20がどちらも残っている(Lost Updateが起きない)");
}

/// Dirty Read: Table Lockのもとでは起きない。
///
/// T1が`UPDATE`した時点でaccountsテーブルのExclusiveロックを持つため、T2の
/// `SELECT`(Sharedロックを要求する)はブロックされる。T1が`ROLLBACK`して
/// ロックを手放すまで、T2はT1の未コミットの変更を一切読めない。
#[test]
fn dirty_read_is_prevented_by_table_lock() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    // T1は結局ロールバックし、ロックを手放す。
    db.rollback_tx(t1).unwrap();

    // T2が読めるのは、T1がロールバックしたあとの値(100)だけである。
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let value = int_value(&read, 0, 0);
    db.commit_tx(t2).unwrap();

    assert_eq!(value, 100, "T2は一度もコミットされていない値(70)を読めない(Dirty Readが起きない)");
}

/// Non-repeatable Read: Table Lockのもとでは起きない。
///
/// T1が1回目の`SELECT`でSharedロックを持ち続ける限り、T2の`UPDATE`(Exclusive
/// を要求する)はブロックされたままになる。T1が同じ行を2回目に読むときも、
/// この間T2の書き込みは一度も成立していないので、1回目と同じ値が返る。
#[test]
fn non_repeatable_read_is_prevented_by_table_lock() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&first, 0, 0), 100);

    // T2の更新は、T1がまだSharedロックを持っているためブロックされる。
    assert!(matches!(
        db.execute_in_tx(&t2, "UPDATE accounts SET balance = 70 WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    // T1が(コミットもロールバックもせず)同じ行をもう一度読む。
    let second = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let second_value = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_value, 100, "同じT1の中での2回目の読み取りが1回目と同じ値のまま(Non-repeatable Readが起きない)");

    // T1がコミットしてロックを手放したので、T2は再試行して成功する。
    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();
}

/// Phantom: Table Lockのもとでは起きない。
///
/// `INSERT`もテーブル全体のExclusiveロックを要求するため、T1が集計用の
/// Sharedロックを持っている間、T2の`INSERT`はブロックされる。この点が
/// Tuple Lockとの違いである(`tests/interleave_disk.rs`を参照)。
#[test]
fn phantom_read_is_prevented_by_table_lock() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    assert_eq!(int_value(&first, 0, 0), 2);

    assert!(matches!(
        db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)"),
        Err(DbError::WouldBlock)
    ));

    let second = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let second_count = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_count, 2, "T2の挿入がブロックされたままなので、2回目の集計も1回目と同じ(Phantomが起きない)");

    db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)").unwrap();
    db.commit_tx(t2).unwrap();
}

/// ハーネス自体の検証: 3本以上のトランザクションを同時に開き、
/// commit_tx・rollback_txを混ぜて呼んでも、それぞれ独立に効くことを確認する。
///
/// Table Lockの粒度ではテーブルを共有する2本のトランザクションは互いに
/// ブロックし合ってしまうため、3本をそれぞれ別のテーブルへ割り当て、
/// ロックが一切競合しない形でハーネスの独立性だけを確認する。
#[test]
fn three_transactions_can_be_interleaved_independently() {
    let mut db = temp_db();
    db.execute("CREATE TABLE ta (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("CREATE TABLE tb (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("CREATE TABLE tc (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO ta VALUES (1, 0)").unwrap();
    db.execute("INSERT INTO tb VALUES (1, 0)").unwrap();
    db.execute("INSERT INTO tc VALUES (1, 0)").unwrap();

    let a = db.begin_tx();
    let b = db.begin_tx();
    let c = db.begin_tx();

    db.execute_in_tx(&a, "UPDATE ta SET v = 1 WHERE id = 1").unwrap();
    db.execute_in_tx(&b, "UPDATE tb SET v = 2 WHERE id = 1").unwrap();
    db.execute_in_tx(&c, "UPDATE tc SET v = 3 WHERE id = 1").unwrap();
    db.execute_in_tx(&a, "UPDATE ta SET v = 10 WHERE id = 1").unwrap();

    db.commit_tx(a).unwrap();
    db.rollback_tx(b).unwrap();
    db.commit_tx(c).unwrap();

    assert_eq!(int_value(&db.execute("SELECT v FROM ta").unwrap(), 0, 0), 10, "aはコミット済み(最後の書き込みが残る)");
    assert_eq!(int_value(&db.execute("SELECT v FROM tb").unwrap(), 0, 0), 0, "bはロールバック済み(元の値へ戻る)");
    assert_eq!(int_value(&db.execute("SELECT v FROM tc").unwrap(), 0, 0), 3, "cはコミット済み");
}
