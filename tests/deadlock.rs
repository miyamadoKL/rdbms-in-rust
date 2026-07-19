//! デッドロックの検出とVictim Selection(第32章)。
//!
//! 第31章は、2本のトランザクションが互いの持つロックを待ち合う状況(相互待ち)
//! が起きても、`LockManager`自身はそれを解決しないことを
//! `mutual_wait_leaves_both_transactions_blocked_without_detection`
//! (`src/lock_manager.rs`)で確認していた。この章の`Database`は、ロックの
//! 要求が`Blocked`になるたびWait-for Graphを調べ、循環(デッドロック)が
//! 見つかればVictim Selection(循環の中で最も新しい`TransactionId`を選ぶ)で
//! 片方を強制的に`Aborted`へ倒す。このファイルは、その反転
//! (相互待ちのまま止まらず、片方がAbortされてもう片方が完走する)を確認する。

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

/// 要求元自身が循環を閉じ、かつ循環の中で最も新しいトランザクションでも
/// あるとき、Victimは要求元と一致する。
///
/// T1がtable `tb`を要求した時点(1回目)ではまだ循環が無い(T2は誰も
/// 待っていない)ので、ただの`WouldBlock`になる。続けてT2がtable `ta`を
/// 要求すると、T1⇄T2の循環が閉じる。この2件目の要求を出したT2自身が循環の
/// 中で最も新しい(`TransactionId`が大きい)トランザクションなので、Victimに
/// 選ばれるのはT2であり、要求元と一致する。T2はその場で`DeadlockDetected`を
/// 受け取り、それ以降は`ROLLBACK`(または`DeadlockDetected`をもう一度返す
/// 操作)しか受け付けない。T2の未コミットの書き込みはUndoで取り消され、
/// 手放したロックをT1が引き継いで完走する。
#[test]
fn requester_becomes_its_own_victim_when_it_closes_the_cycle() {
    let mut db = temp_db();
    db.execute("CREATE TABLE ta (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("CREATE TABLE tb (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO ta VALUES (1, 0)").unwrap();
    db.execute("INSERT INTO tb VALUES (1, 0)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    db.execute_in_tx(&t1, "UPDATE ta SET v = 1 WHERE id = 1").unwrap();
    db.execute_in_tx(&t2, "UPDATE tb SET v = 2 WHERE id = 1").unwrap();

    // T1がtbを欲しがるが、T2が持っている。循環はまだ閉じていない
    // (T2は誰も待っていない)ので、ただのWouldBlockになる。
    assert!(matches!(db.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1"), Err(DbError::WouldBlock)));

    // T2がtaを欲しがると、T1⇄T2の循環が閉じる。この要求を出したT2自身が
    // 循環の中で最も新しいトランザクションなので、Victimに選ばれる。
    assert!(matches!(db.execute_in_tx(&t2, "UPDATE ta SET v = 20 WHERE id = 1"), Err(DbError::DeadlockDetected)));

    // T2はもうROLLBACKしか受け付けない。触れるたびDeadlockDetectedが返る。
    assert!(matches!(db.execute_in_tx(&t2, "SELECT 1"), Err(DbError::DeadlockDetected)));
    assert!(matches!(db.commit_tx(t2), Err(DbError::DeadlockDetected)));

    // T2が手放したtbのロックを、待っていたT1が引き継ぐ。さっき
    // WouldBlockだった同じ文を再試行すると、今度は成功する
    // (T1がtbのExclusiveロックをすでに引き継いでいるため、他のトランザクション
    // からのSharedロック要求は今度はT1自身とぶつかる側になる。ここではT2の
    // 未コミットの書き込み(tb.v = 2)がUndoで取り消されていることを、T1自身の
    // 読み取りで確認する)。
    let read = db.execute_in_tx(&t1, "SELECT v FROM tb WHERE id = 1").unwrap();
    assert_eq!(int_value(&read, 0, 0), 0, "Victimの未コミット書き込みは取り消される");
    db.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1").unwrap();
    db.commit_tx(t1).unwrap();

    assert_eq!(int_value(&db.execute("SELECT v FROM ta").unwrap(), 0, 0), 1, "T1のtaへの更新は生き残る");
    assert_eq!(int_value(&db.execute("SELECT v FROM tb").unwrap(), 0, 0), 10, "T1がtbも更新できた");
}

/// Victimは要求元と一致するとは限らない。循環の中で最も新しいトランザク
/// ションが、たまたま要求元でないこともある。
///
/// この例ではT2がtable `ta`を先に要求してBlockedになる(循環はまだ無い)。
/// 続けてT1がtable `tb`を要求すると循環が閉じるが、循環の中で最も新しい
/// トランザクションはT2である(T1より後に`BEGIN`した)。Victimに選ばれる
/// のはT2で、要求元のT1ではない。T1の要求は、T2が手放したロックをその場で
/// 引き継いでそのまま`Ok`になる(T1は自分がデッドロックの解決に巻き込まれた
/// ことにすら気づかない)。T2の側は、次にこのトランザクションへ触れたとき
/// (ここでは`commit_tx`)に初めて`DeadlockDetected`を受け取る。
#[test]
fn a_different_transaction_can_be_chosen_as_the_victim() {
    let mut db = temp_db();
    db.execute("CREATE TABLE ta (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("CREATE TABLE tb (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO ta VALUES (1, 0)").unwrap();
    db.execute("INSERT INTO tb VALUES (1, 0)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();

    db.execute_in_tx(&t1, "UPDATE ta SET v = 1 WHERE id = 1").unwrap();
    db.execute_in_tx(&t2, "UPDATE tb SET v = 2 WHERE id = 1").unwrap();

    // T2がtaを欲しがるが、まだ循環は無い(T1は誰も待っていない)。
    assert!(matches!(db.execute_in_tx(&t2, "UPDATE ta SET v = 20 WHERE id = 1"), Err(DbError::WouldBlock)));

    // T1がtbを欲しがると循環が閉じる。要求元はT1だが、循環内で最も新しい
    // T2がVictimに選ばれるため、T1の要求はそのまま成功する。
    db.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1").unwrap();
    db.commit_tx(t1).unwrap();

    assert_eq!(int_value(&db.execute("SELECT v FROM ta").unwrap(), 0, 0), 1, "T1のtaへの更新は生き残る");
    assert_eq!(int_value(&db.execute("SELECT v FROM tb").unwrap(), 0, 0), 10, "T1がtbも更新できた");

    // T2は自分がVictimになったことをまだ知らない。次に触れた操作
    // (ここではcommit_tx)で初めてDeadlockDetectedを受け取る。
    assert!(matches!(db.commit_tx(t2), Err(DbError::DeadlockDetected)));
}

/// 3本のトランザクションが環状に待ち合う場合も検出できる
/// (T1はT2のtbを、T2はT3のtcを、T3はT1のtaを待つ)。
///
/// この構成では、循環を最後に閉じる要求元(T3)自身が循環内で最も新しい
/// トランザクションになる。T3の要求は`DeadlockDetected`になり、T3が
/// 手放した`tc`をT2が引き継いで完走し、続けてT2が手放した`tb`をT1が
/// 引き継いで完走する。
#[test]
fn a_three_way_cycle_is_also_detected() {
    let mut db = temp_db();
    for table in ["ta", "tb", "tc"] {
        db.execute(&format!("CREATE TABLE {table} (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)")).unwrap();
        db.execute(&format!("INSERT INTO {table} VALUES (1, 0)")).unwrap();
    }

    let t1 = db.begin_tx();
    let t2 = db.begin_tx();
    let t3 = db.begin_tx();

    db.execute_in_tx(&t1, "UPDATE ta SET v = 1 WHERE id = 1").unwrap();
    db.execute_in_tx(&t2, "UPDATE tb SET v = 2 WHERE id = 1").unwrap();
    db.execute_in_tx(&t3, "UPDATE tc SET v = 3 WHERE id = 1").unwrap();

    // T1はtb(T2が保持)を、T2はtc(T3が保持)を欲しがる。どちらの時点でも
    // まだ循環は閉じていない。
    assert!(matches!(db.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1"), Err(DbError::WouldBlock)));
    assert!(matches!(db.execute_in_tx(&t2, "UPDATE tc SET v = 20 WHERE id = 1"), Err(DbError::WouldBlock)));

    // T3がta(T1が保持)を欲しがると、T1→T2→T3→T1の循環が閉じる。要求元の
    // T3が循環内で最も新しいトランザクションなので、VictimはT3自身になる。
    assert!(matches!(db.execute_in_tx(&t3, "UPDATE ta SET v = 30 WHERE id = 1"), Err(DbError::DeadlockDetected)));

    // T3が手放したtcを、待っていたT2が引き継ぐ。
    db.execute_in_tx(&t2, "UPDATE tc SET v = 20 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();

    // T2がコミットしてtbを手放したので、待っていたT1も引き継いで完走する。
    db.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1").unwrap();
    db.commit_tx(t1).unwrap();

    assert_eq!(int_value(&db.execute("SELECT v FROM ta").unwrap(), 0, 0), 1, "T1のtaへの更新は生き残る");
    assert_eq!(int_value(&db.execute("SELECT v FROM tb").unwrap(), 0, 0), 10, "T1がtbも更新できた");
    assert_eq!(int_value(&db.execute("SELECT v FROM tc").unwrap(), 0, 0), 20, "T2がtcを更新できた");
    assert!(matches!(db.commit_tx(t3), Err(DbError::DeadlockDetected)), "T3はVictimのまま");
}

/// FIFOの待ち行列の順序**だけ**が循環を閉じている3本のトランザクションの
/// 循環待ちも検出できる。
///
/// `wait_for_edges`(`src/lock_manager.rs`)は、モードが衝突する保持者への
/// 辺に加えて、待ち行列上で自分の直前に並ぶ要求への辺(FIFOで追い越せない
/// という依存)も返すようになった。この辺が無いと、T3の`SELECT`はT1の
/// 保持する`Shared`と両立するため衝突辺が生まれず、実際には存在する循環
/// (T1→T3→T2→T1)を見逃す(この章のレビューで実際に指摘された不具合)。
///
/// - T1が`ta`に`Shared`を持つ。
/// - T2が`ta`に`Exclusive`を要求してBlocked(T1と衝突、循環はまだ無い)。
/// - T3が`tb`に`Exclusive`を獲得したあと、`ta`に`Shared`を要求する。T1とは
///   両立するが、待ち行列にはすでにT2がいるためFIFOでT2の後ろに並ぶ
///   (循環はまだ無い)。
/// - T1が`tb`に`Exclusive`を要求する。T3が保持する`tb`と衝突し(辺T1→T3)、
///   これに「T3はT2の後ろで待っている」というFIFOの辺(T3→T2)と、
///   「T2はT1と衝突している」という辺(T2→T1)が合わさって、T1→T3→T2→T1の
///   循環が閉じる。この要求を出したT1自身が循環内で最も新しいトランザク
///   ション(3番目に`begin_tx`した)なので、VictimはT1自身になる。
#[test]
fn a_cycle_closed_only_by_fifo_wait_queue_order_is_also_detected() {
    let mut db = temp_db();
    db.execute("CREATE TABLE ta (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("CREATE TABLE tb (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO ta VALUES (1, 0)").unwrap();
    db.execute("INSERT INTO tb VALUES (1, 0)").unwrap();

    // T1がta・tbへ触れる前にT2・T3をbegin_txしておき、循環を最後に閉じる
    // T1自身が(TransactionIdが最大の)Victimになるようにする
    // (begin_txの順序と文の実行順序は独立である、`TransactionId`は
    // `begin_tx`した順にしか大小関係を持たない)。
    let t2 = db.begin_tx();
    let t3 = db.begin_tx();
    let t1 = db.begin_tx();

    // T1がtaにSharedを持つ。
    db.execute_in_tx(&t1, "SELECT * FROM ta").unwrap();

    // T2のExclusiveはT1のSharedと衝突してBlocked。循環はまだ無い。
    assert!(matches!(db.execute_in_tx(&t2, "UPDATE ta SET v = 2 WHERE id = 1"), Err(DbError::WouldBlock)));

    // T3はtbのExclusiveを獲得する(誰も持っていない)。
    db.execute_in_tx(&t3, "UPDATE tb SET v = 3 WHERE id = 1").unwrap();

    // T3のSharedはT1の保持と両立するが、待ち行列のT2を追い越せずBlocked。
    // モードの衝突は無いので、これはFIFOの順序だけによるブロックである。
    assert!(matches!(db.execute_in_tx(&t3, "SELECT * FROM ta"), Err(DbError::WouldBlock)));

    // T1がtbを要求すると、T3の保持と衝突する(辺T1→T3)。これにFIFOの辺
    // (T3→T2)とモード衝突の辺(T2→T1)が合わさって循環が閉じる。
    assert!(matches!(db.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1"), Err(DbError::DeadlockDetected)));

    // T1はVictimになったので、以降ROLLBACKしか受け付けない。
    assert!(matches!(db.execute_in_tx(&t1, "SELECT 1"), Err(DbError::DeadlockDetected)));
    db.rollback_tx(t1).unwrap();

    // T1が手放したtaのShared(すでに解放済み)により、待ち行列の先頭T2が
    // 昇格し、続けてT3もtaのSharedを獲得できる。
    db.execute_in_tx(&t2, "UPDATE ta SET v = 2 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();
    let read = db.execute_in_tx(&t3, "SELECT v FROM ta WHERE id = 1").unwrap();
    assert_eq!(int_value(&read, 0, 0), 2, "T2の更新をT3が読める");
    db.commit_tx(t3).unwrap();

    assert_eq!(int_value(&db.execute("SELECT v FROM ta").unwrap(), 0, 0), 2, "T2のtaへの更新は生き残る");
    assert_eq!(int_value(&db.execute("SELECT v FROM tb").unwrap(), 0, 0), 3, "T3のtbへの更新は生き残る(T1はUndoされた)");
}
