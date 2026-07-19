//! 決定的インターリーブテストハーネス(第30章)。
//!
//! Buffer Pool・B+Treeがスレッドセーフになるのは第35章であり、それより前の章
//! (第31〜34章)では実スレッドを立てて複数のトランザクションを競合させる
//! テストを書けない。この章は、実スレッドの代わりに「1つの`Database`の上で、
//! 複数のトランザクションの文を書いた順序どおりに交互実行する」という形で
//! インターリーブを再現する。順序はすべてこのファイルのRustコードが明示的に
//! 書き下すため、実行結果は毎回決定的である(スレッドスケジューラの気まぐれに
//! 左右されない)。
//!
//! ここで使う`Database::begin_tx`・`execute_in_tx`・`commit_tx`・`rollback_tx`
//! は、通常のSQL経路(`db.execute("BEGIN")`)とは別の、複数のトランザクションを
//! 同時に開けるハーネス専用のAPIである。`Database`の通常の`tx`フィールドは
//! `Active`なトランザクションを1本しか持てないため、複数のトランザクションを
//! 行き来しながら進めるにはこの別経路が要る(詳しくは`src/database.rs`の
//! 「決定的インターリーブテストハーネス専用の内部API」を参照)。
//!
//! この章の時点でminidbが持つ並行制御は「無い」。`INSERT`・`UPDATE`・`DELETE`
//! はどのトランザクションの変更も即座に共有された`Backend`へ反映し、他の
//! トランザクションはそれを`ROLLBACK`より前から見ることができる。以下の4つの
//! テストは、その結果として実際に起きる異常(Lost Update、Dirty Read、
//! Non-repeatable Read、Phantom)を「起きる」ことを`assert`する形で固定する。
//! 第31章のLock Manager、第32章のIsolation Levelが実装されると、この4つの
//! `assert`は反転する(異常が「起きない」ことを確認する形に書き換わる)。

mod common;

use common::temp_db;
use minidb::Value;

fn int_value(result: &minidb::QueryResult, row: usize, col: usize) -> i64 {
    match &result.rows()[row].values()[col] {
        Value::BigInt(n) => *n,
        other => panic!("BigIntを期待したが{other:?}が返った"),
    }
}

/// Lost Update: 2つのトランザクションが同じ行を「読んで、計算して、書く」を
/// 別々に行うと、後にコミットした側が先の更新を丸ごと上書きしてしまう。
///
/// アプリケーション側の`balance + 10`のような相対更新ではなく、それぞれの
/// トランザクションが自分の読んだ値をもとに計算した**リテラルな**新しい値を
/// `UPDATE`に書く(「読んで、計算して、書く」という、実際にLost Updateを
/// 引き起こすアプリケーションコードのパターンをそのまま再現するため)。
#[test]
fn lost_update_is_not_prevented_yet() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    // 1. T1が残高を読む(100)。
    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t1_seen = int_value(&read, 0, 0);
    assert_eq!(t1_seen, 100);

    // 2. T2も同じ残高を読む(100、T1がまだ書いていないので同じ値)。
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t2_seen = int_value(&read, 0, 0);
    assert_eq!(t2_seen, 100);

    // 3. T2が「100 + 20」を計算し、書いてコミットする。
    db.execute_in_tx(&t2, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t2_seen + 20)).unwrap();
    db.commit_tx(t2).unwrap();

    // 4. T1は手順1で読んだ古い値(100)をもとに「100 + 10」を計算し、書いて
    //    コミットする。T1はT2の書き込みを一度も読み直していない。
    db.execute_in_tx(&t1, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t1_seen + 10)).unwrap();
    db.commit_tx(t1).unwrap();

    // 正しく直列化されていれば130(100+20+10、順序によらず両方の更新が残る)
    // になるはずだが、実際にはT1の書き込みがT2の書き込みを丸ごと上書きし、
    // T2が足した20はどこにも残らない。
    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&result, 0, 0), 110, "T2が足した20が失われている(Lost Update)");
}

/// Dirty Read: T1がまだコミットしていない変更を、T2が読めてしまう。
/// T1が結局`ROLLBACK`すれば、T2が読んだ値は最初から存在しなかったことになる。
#[test]
fn dirty_read_is_not_prevented_yet() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    // T1が残高を減らすが、まだコミットしていない。
    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    // T2は、T1がコミットしたかどうかに関係なく、その未コミットの値を読める。
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let dirty_value = int_value(&read, 0, 0);

    // T1は結局ロールバックする。
    db.rollback_tx(t1).unwrap();
    db.commit_tx(t2).unwrap();

    assert_eq!(dirty_value, 70, "T2はT1のロールバックされる運命の値(70)を読めてしまっている(Dirty Read)");
    // ロールバック後、実際の残高は100へ戻っている。T2が読んだ70は、
    // 一度もコミットされたことのない値だった。
    let after = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&after, 0, 0), 100);
}

/// Non-repeatable Read: 同じトランザクションの中で同じ行を2回読むと、
/// 間に割り込んだ別のトランザクションのコミットによって値が変わってしまう。
#[test]
fn non_repeatable_read_is_not_prevented_yet() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    // T1が1回目の読み取り。
    let first = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let first_value = int_value(&first, 0, 0);

    // T2が割り込んで更新し、コミットする。
    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();

    // T1が(コミットもロールバックもせず)同じ行をもう一度読む。
    let second = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let second_value = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(first_value, 100);
    assert_eq!(second_value, 70, "同じT1の中での2回目の読み取りが、1回目(100)と異なる値(70)になっている(Non-repeatable Read)");
}

/// Phantom: 同じトランザクションの中で同じ`WHERE`条件を2回集計すると、
/// 間に割り込んだ別のトランザクションが挿入した行が、2回目にだけ現れてしまう。
#[test]
fn phantom_read_is_not_prevented_yet() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    // T1が「残高が40より大きい行」を1回目に数える。
    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let first_count = int_value(&first, 0, 0);

    // T2が、同じ条件に一致する新しい行を挿入してコミットする。
    db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)").unwrap();
    db.commit_tx(t2).unwrap();

    // T1が同じ条件で2回目を数える。
    let second = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let second_count = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(first_count, 2);
    assert_eq!(second_count, 3, "同じT1の中での2回目の集計に、T2が挿入した行(幻)が現れている(Phantom Read)");
}

/// ハーネス自体の検証: 3本以上のトランザクションを同時に開き、
/// commit_tx・rollback_txを混ぜて呼んでも、それぞれ独立に効くことを確認する。
#[test]
fn three_transactions_can_be_interleaved_independently() {
    let mut db = temp_db();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO t VALUES (1, 0), (2, 0), (3, 0)").unwrap();

    let a = db.begin_tx();
    let b = db.begin_tx();
    let c = db.begin_tx();

    db.execute_in_tx(&a, "UPDATE t SET v = 1 WHERE id = 1").unwrap();
    db.execute_in_tx(&b, "UPDATE t SET v = 2 WHERE id = 2").unwrap();
    db.execute_in_tx(&c, "UPDATE t SET v = 3 WHERE id = 3").unwrap();
    db.execute_in_tx(&a, "UPDATE t SET v = 10 WHERE id = 1").unwrap();

    db.commit_tx(a).unwrap();
    db.rollback_tx(b).unwrap();
    db.commit_tx(c).unwrap();

    let result = db.execute("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(int_value(&result, 0, 1), 10, "aはコミット済み(最後の書き込みが残る)");
    assert_eq!(int_value(&result, 1, 1), 0, "bはロールバック済み(元の値へ戻る)");
    assert_eq!(int_value(&result, 2, 1), 3, "cはコミット済み");
}
