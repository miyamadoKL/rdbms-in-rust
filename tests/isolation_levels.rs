//! 4分離レベル × 4異常のマトリクス(第32章)。
//!
//! 第30章は`tests/interleave.rs`・`tests/interleave_disk.rs`で、Lost Update・
//! Dirty Read・Non-repeatable Read・Phantomという4つの異常を固定した。第31章の
//! Strict 2PLは、実質的に`RepeatableRead`(Shared LockもCOMMITまで保持する
//! 規律)1本だけを実装しており、4つのうち3つ(Memoryバックエンドでは4つ全部)を
//! 防いだ。この章は分離レベルを`READ UNCOMMITTED`・`READ COMMITTED`・
//! `REPEATABLE READ`・`SERIALIZABLE`の4段階へ分け、レベルごとにどの異常が
//! 起き、どれが防がれるかを固定する。
//!
//! Memoryバックエンド(ロックの粒度はテーブル単位)でのマトリクスは次のとおり。
//!
//! | レベル | Dirty Read | Non-repeatable Read | Lost Update | Phantom |
//! |---|---|---|---|---|
//! | Read Uncommitted | 起きる | 起きる | 起きる | 起きる |
//! | Read Committed | 防がれる | 起きる | 起きる | 起きる |
//! | Repeatable Read | 防がれる | 防がれる | 防がれる | 防がれる |
//! | Serializable | 防がれる | 防がれる | 防がれる | 防がれる |
//!
//! `RepeatableRead`と`Serializable`がMemoryバックエンドで区別できないのは
//! 偶然ではない。テーブル単位のロックはもとから`INSERT`もテーブル全体の
//! Exclusiveを要求するため、Phantomの土台(まだ存在しない行への挿入だけが
//! 誰にも守られない、というTuple Lockの非対称性)自体が最初から無い
//! (`crate::database`モジュール冒頭「ロックの粒度」を参照)。この2レベルの
//! 違いが意味を持つのはDiskバックエンド(Tuple Lock)のときだけであり、
//! それは本ファイル末尾の`Serializable`のテストで確認する。

mod common;

use common::{temp_db, temp_db_path};
use minidb::error::DbError;
use minidb::{Database, IsolationLevel, Value};

fn int_value(result: &minidb::QueryResult, row: usize, col: usize) -> i64 {
    match &result.rows()[row].values()[col] {
        Value::BigInt(n) => *n,
        other => panic!("BigIntを期待したが{other:?}が返った"),
    }
}

// ==== READ UNCOMMITTED: 4つとも起きる ====

#[test]
fn read_uncommitted_allows_dirty_read() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx_with_isolation(IsolationLevel::ReadUncommitted);

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    // T2はREAD UNCOMMITTEDなので読み取りロックを一切取らず、T1のExclusive
    // ロックとぶつからずに読める。
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let dirty_value = int_value(&read, 0, 0);
    db.commit_tx(t2).unwrap();

    db.rollback_tx(t1).unwrap();

    assert_eq!(dirty_value, 70, "T2はT1のロールバックされる運命の値(70)を読めてしまう(Dirty Read)");
}

#[test]
fn read_uncommitted_allows_non_repeatable_read() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::ReadUncommitted);
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&first, 0, 0), 100);

    // T1が読み取りロックを取らないため、T2のUPDATEはぶつからずに成功する。
    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();

    let second = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let second_value = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_value, 999, "同じT1の中で1回目(100)と2回目(999)の読み取りが食い違う(Non-repeatable Read)");
}

#[test]
fn read_uncommitted_allows_lost_update() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::ReadUncommitted);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::ReadUncommitted);

    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t1_seen = int_value(&read, 0, 0);

    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t2_seen = int_value(&read, 0, 0);

    db.execute_in_tx(&t2, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t2_seen + 20)).unwrap();
    db.commit_tx(t2).unwrap();

    // T1は自分が最初に読んだ100を根拠に計算した110を書く。読み取りロックが
    // 無いため、この書き込みはT2のExclusiveロック(すでに解放済み)ともT2の
    // 書き込み内容とも一切照合されない。
    db.execute_in_tx(&t1, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t1_seen + 10)).unwrap();
    db.commit_tx(t1).unwrap();

    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&result, 0, 0), 110, "T2の+20がT1の書き込みで上書きされて消えている(Lost Update)");
}

#[test]
fn read_uncommitted_allows_phantom_read() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::ReadUncommitted);
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    assert_eq!(int_value(&first, 0, 0), 2);

    // T1が読み取りロックを取らないため、T2のINSERTはぶつからずに成功する。
    db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)").unwrap();
    db.commit_tx(t2).unwrap();

    let second = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let second_count = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_count, 3, "T2が挿入した行(幻)が2回目の集計に現れている(Phantom)");
}

// ==== READ COMMITTED: Dirty Readだけ防がれる ====

#[test]
fn read_committed_prevents_dirty_read() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    // T2はREAD COMMITTEDでも読み取りの瞬間にはSharedロックを要求するため、
    // T1がまだ持っているExclusiveロックとぶつかってBlockedになる。
    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    db.rollback_tx(t1).unwrap();

    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let value = int_value(&read, 0, 0);
    db.commit_tx(t2).unwrap();

    assert_eq!(value, 100, "T2は一度もコミットされていない値(70)を読めない(Dirty Readが起きない)");
}

#[test]
fn read_committed_allows_non_repeatable_read() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&first, 0, 0), 100);

    // T1のSharedロックは1回目のSELECTの終わりですでに解放されているため、
    // T2のUPDATEはぶつからずに成功する。
    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();

    let second = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let second_value = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_value, 999, "同じT1の中で1回目(100)と2回目(999)の読み取りが食い違う(Non-repeatable Read)");
}

#[test]
fn read_committed_allows_lost_update() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);

    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t1_seen = int_value(&read, 0, 0);
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t2_seen = int_value(&read, 0, 0);

    db.execute_in_tx(&t2, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t2_seen + 20)).unwrap();
    db.commit_tx(t2).unwrap();

    // T1・T2どちらのSharedロックも読み取りの直後に解放済みなので、T1の
    // UPDATEはT2の書き込みとぶつからず、T1が最初に読んだ100を根拠にした
    // 計算のまま上書きしてしまう。
    db.execute_in_tx(&t1, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t1_seen + 10)).unwrap();
    db.commit_tx(t1).unwrap();

    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&result, 0, 0), 110, "T2の+20がT1の書き込みで上書きされて消えている(Lost Update)");
}

#[test]
fn read_committed_allows_phantom_read() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);
    let t2 = db.begin_tx();

    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    assert_eq!(int_value(&first, 0, 0), 2);

    // T1のSharedロックは1回目の集計の終わりですでに解放されているため、
    // T2のINSERTはぶつからずに成功する。
    db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)").unwrap();
    db.commit_tx(t2).unwrap();

    let second = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let second_count = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();

    assert_eq!(second_count, 3, "T2が挿入した行(幻)が2回目の集計に現れている(Phantom)");
}

/// READ COMMITTEDが文末に解放してよいのは、その文で**新規に**取得した
/// Sharedロックだけである。T1が`UPDATE`で獲得したExclusiveロックは、直後の
/// `SELECT`(同じテーブルを読むだけ)が終わってもCOMMIT・ABORTまで保持され
/// 続けなければならない。
///
/// 修正前は、`acquire_scan_locks`が「この`SELECT`のために獲得しようとした
/// 鍵」をすべて文末解放の対象にしており、T1が`UPDATE`からすでに持っていた
/// Exclusiveロックまで一緒に解放してしまっていた。その結果、T1がまだ
/// `COMMIT`していないのに、T2の同じ行への`UPDATE`が通ってしまう(この章の
/// レビューで実際に指摘された不具合)。
#[test]
fn read_committed_select_after_update_does_not_release_the_update_s_exclusive_lock() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);

    // T1がid=1をUPDATEし、Exclusiveロックを獲得する。
    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    // 同じテーブルへのSELECTは、READ COMMITTEDの規律どおり文末でShared
    // ロックを解放する。しかし、これはT1が「この文で新規に取得した」
    // Sharedロックの話であり、先行するUPDATEのExclusiveロックには触れない。
    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&read, 0, 0), 70, "T1は自分の未コミットの書き込みを読める");

    // T1がまだCOMMITしていないので、T2の同じ行へのUPDATEはExclusiveロック
    // とぶつかってBlockedのままでなければならない。
    assert!(
        matches!(db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1"), Err(DbError::WouldBlock)),
        "T1のUPDATEが持つExclusiveロックが、直後のSELECTで誤って解放されている"
    );

    db.commit_tx(t1).unwrap();

    // T1がCOMMITして初めて、T2の書き込みが通る。
    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();

    assert_eq!(int_value(&db.execute("SELECT balance FROM accounts").unwrap(), 0, 0), 999);
}

/// READ COMMITTEDのSELECTが一度`WouldBlock`で待たされ、先行トランザクション
/// のCOMMIT後にShared Lockが**待ち行列から**付与されたケースでも、その
/// Sharedロックは文末で正しく解放されなければならない。
///
/// 修正前は、獲得する**前**に`held_mode`を見て「すでに保持していれば既存の
/// ロック」と判定していた。ところが、T1のCOMMITで待ち行列からT2へ昇格した
/// Sharedロックは、T2がこの文を再試行して`acquire`を呼ぶ**前から**すでに
/// `Some(Shared)`になっている。そのため「以前から持っていた」と誤判定され、
/// 文末解放から漏れる(この章のレビュー2巡目で実際に指摘された不具合)。
/// この漏れの結果、T2のSELECTが完了したあとも第三のトランザクションT3の
/// `UPDATE`が`WouldBlock`のままになってしまう。
#[test]
fn read_committed_select_granted_after_waiting_is_still_released_at_statement_end() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);
    let t3 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);

    // T1がExclusiveを獲得する。
    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    // T2のSELECTはT1のExclusiveとぶつかりWouldBlock(待ち行列に並ぶ)。
    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    // T1がCOMMITすると、待ち行列に並んでいたT2のSharedが即座に昇格する
    // (T2がまだこの文を再試行してすらいない時点で、である)。
    db.commit_tx(t1).unwrap();

    // T2がSELECTを再試行すると、今度は成功する。
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&read, 0, 0), 70);

    // READ COMMITTEDの規律どおり、この文の終わりでT2のSharedは解放されて
    // いるはずである。T3のUPDATEはブロックされずに進めなければならない。
    assert!(
        db.execute_in_tx(&t3, "UPDATE accounts SET balance = 999 WHERE id = 1").is_ok(),
        "待ち行列から付与されたSharedロックが、文末で解放されずに残っている"
    );
    db.commit_tx(t2).unwrap();
    db.commit_tx(t3).unwrap();

    assert_eq!(int_value(&db.execute("SELECT balance FROM accounts").unwrap(), 0, 0), 999);
}

// ==== REPEATABLE READ: 4つとも防がれる(Memoryバックエンド) ====

#[test]
fn repeatable_read_prevents_dirty_read() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::RepeatableRead);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::RepeatableRead);

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    db.rollback_tx(t1).unwrap();
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let value = int_value(&read, 0, 0);
    db.commit_tx(t2).unwrap();
    assert_eq!(value, 100, "Dirty Readが起きない");
}

#[test]
fn repeatable_read_prevents_non_repeatable_read() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::RepeatableRead);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::RepeatableRead);

    let first = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&first, 0, 0), 100);

    // T1のSharedロックがCOMMITまで保持されているので、T2のUPDATEはブロックされる。
    assert!(matches!(
        db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    let second = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let second_value = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();
    assert_eq!(second_value, 100, "Non-repeatable Readが起きない");

    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();
}

#[test]
fn repeatable_read_prevents_lost_update() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::RepeatableRead);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::RepeatableRead);

    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    let t1_seen = int_value(&read, 0, 0);
    db.execute_in_tx(&t1, &format!("UPDATE accounts SET balance = {} WHERE id = 1", t1_seen + 10)).unwrap();

    // T1がExclusiveロックを持っているため、T2の読み取りはブロックされる。
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
}

#[test]
fn repeatable_read_prevents_phantom_read_on_memory_backend() {
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::RepeatableRead);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::RepeatableRead);

    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    assert_eq!(int_value(&first, 0, 0), 2);

    // MemoryバックエンドのINSERTもテーブル全体のExclusiveを要求するため、
    // T1が集計用のSharedを持っている間はブロックされる。
    assert!(matches!(
        db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)"),
        Err(DbError::WouldBlock)
    ));

    let second = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    let second_count = int_value(&second, 0, 0);
    db.commit_tx(t1).unwrap();
    assert_eq!(second_count, 2, "Phantomが起きない");

    db.execute_in_tx(&t2, "INSERT INTO accounts VALUES (3, 200)").unwrap();
    db.commit_tx(t2).unwrap();
}

// ==== SERIALIZABLE: 4つとも防がれる。DiskバックエンドでのPhantom防止が
// RepeatableReadとの違い ====

#[test]
fn serializable_prevents_the_same_three_anomalies_as_repeatable_read_on_memory() {
    // MemoryバックエンドではSerializableとRepeatableReadに違いが無い
    // (テーブル単位のロックがもとからPhantomの土台を作らない、モジュール
    // 冒頭を参照)。ここではDirty Read・Lost Updateの2つだけを代表として
    // 確認し、残りはRepeatableReadの節がすでに確認済みである。
    let mut db = temp_db();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::Serializable);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::Serializable);

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));
    db.rollback_tx(t1).unwrap();
    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&read, 0, 0), 100, "Dirty Readが起きない");
    db.commit_tx(t2).unwrap();
}

fn accounts_disk_db(name: &str) -> (Database, std::path::PathBuf) {
    let path = temp_db_path(name);
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    (db, path)
}

/// `tests/interleave_disk.rs`の`phantom_read_is_not_prevented_by_tuple_lock`
/// (`RepeatableRead`、Tuple Lockだけでは防げない)を、この章の`Serializable`で
/// 反転させる。
///
/// `Serializable`のSELECTはTuple Lockに加えて`LockKey::Table`にもSharedを
/// 取り、`Serializable`のINSERTはその`LockKey::Table`にExclusiveを要求する
/// ため、この1本が衝突してPhantomを防ぐ(`src/database.rs`の
/// `acquire_scan_locks`・`run_insert`のドキュメントを参照)。
#[test]
fn serializable_prevents_phantom_read_on_disk_backend() {
    let (mut db, path) = accounts_disk_db("isolation-serializable-phantom");
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::Serializable);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::Serializable);

    let first = db.execute_in_tx(&t1, "SELECT COUNT(*) FROM accounts WHERE balance > 40").unwrap();
    assert_eq!(int_value(&first, 0, 0), 2);

    // RepeatableReadと違い、T2のINSERTはT1のTable Sharedロックとぶつかって
    // ブロックされる。
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
    std::fs::remove_file(&path).unwrap();
}

/// `Serializable`でも、異なる行を対象にする2本の`UPDATE`はDiskバックエンド
/// では互いにブロックし合わない(Table Lockへ昇格するのは`SELECT`・`INSERT`
/// だけであり、`UPDATE`・`DELETE`のロックの規律は分離レベルに関係なく
/// 常にTuple Lockのままである、`acquire_write_locks`のドキュメントを参照)。
#[test]
fn serializable_still_lets_updates_to_different_rows_proceed_concurrently_on_disk() {
    let (mut db, path) = accounts_disk_db("isolation-serializable-different-rows");
    db.execute("INSERT INTO accounts VALUES (1, 100), (2, 50)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::Serializable);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::Serializable);

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 55 WHERE id = 2").unwrap();

    db.commit_tx(t1).unwrap();
    db.commit_tx(t2).unwrap();

    let result = db.execute("SELECT id, balance FROM accounts ORDER BY id").unwrap();
    assert_eq!(int_value(&result, 0, 1), 70);
    assert_eq!(int_value(&result, 1, 1), 55);
    std::fs::remove_file(&path).unwrap();
}

/// `read_committed_select_after_update_does_not_release_the_update_s_exclusive_lock`
/// のDiskバックエンド版。Diskバックエンドは行(`RecordId`)単位でロックする
/// ため、`scan_lock_keys`が返す鍵の型(`LockKey::Tuple`)がMemoryバックエンド
/// (`LockKey::Table`)と異なる。どちらの粒度でも、`acquire_scan_locks`が
/// 「新規取得分だけ」を解放する規律は変わらないことを確認する。
#[test]
fn read_committed_select_after_update_does_not_release_the_update_s_exclusive_lock_on_disk() {
    let (mut db, path) = accounts_disk_db("isolation-read-committed-update-then-select");
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);
    let t2 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    let read = db.execute_in_tx(&t1, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&read, 0, 0), 70, "T1は自分の未コミットの書き込みを読める");

    assert!(
        matches!(db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1"), Err(DbError::WouldBlock)),
        "T1のUPDATEが持つTuple Lockが、直後のSELECTで誤って解放されている"
    );

    db.commit_tx(t1).unwrap();

    db.execute_in_tx(&t2, "UPDATE accounts SET balance = 999 WHERE id = 1").unwrap();
    db.commit_tx(t2).unwrap();

    assert_eq!(int_value(&db.execute("SELECT balance FROM accounts").unwrap(), 0, 0), 999);
    std::fs::remove_file(&path).unwrap();
}

/// `read_committed_select_granted_after_waiting_is_still_released_at_statement_end`
/// のDiskバックエンド版(第5部レビュー2巡目対応)。
#[test]
fn read_committed_select_granted_after_waiting_is_still_released_at_statement_end_on_disk() {
    let (mut db, path) = accounts_disk_db("isolation-read-committed-granted-after-wait");
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);
    let t3 = db.begin_tx_with_isolation(IsolationLevel::ReadCommitted);

    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();

    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    db.commit_tx(t1).unwrap();

    let read = db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(int_value(&read, 0, 0), 70);

    assert!(
        db.execute_in_tx(&t3, "UPDATE accounts SET balance = 999 WHERE id = 1").is_ok(),
        "待ち行列から付与されたTuple Lockが、文末で解放されずに残っている"
    );
    db.commit_tx(t2).unwrap();
    db.commit_tx(t3).unwrap();

    assert_eq!(int_value(&db.execute("SELECT balance FROM accounts").unwrap(), 0, 0), 999);
    std::fs::remove_file(&path).unwrap();
}
