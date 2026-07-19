//! WAL(第33章)がCOMMITの耐久性に実際に効いていることを確認する統合テスト。
//!
//! 最初の2つのテストが、この章の冒頭で再現する「前章までの限界」に対応する。
//! 3つ目以降が、この章の実装がその限界に対して何を変え、何を変えていないかを
//! 確認する。

mod common;

use common::temp_db_path;
use minidb::{Database, Value};

fn wal_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".wal");
    std::path::PathBuf::from(os_string)
}

fn remove_if_exists(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
}

/// 「壊して確認する」: `COMMIT`が成功を返しても、テーブル本体のデータファイルが
/// 同期されているとは限らない。`Database::flush`を一度も呼ばずに`Database`を
/// dropする(プロセスがCOMMIT直後に死んだ状況を模す)と、テーブル本体の
/// ページはこの時点でまだディスクに届いていない。
///
/// 第33章の時点では、この直後に開き直すと行が見当たらなかった(WALを
/// 読み戻す者がいなかったため)。第34章のCrash Recovery
/// (`crate::recovery::recover`、`Database::open`が起動時に自動で呼ぶ)は、
/// この行をWALのInsert/Commitレコードから読んで再現する。このテストの
/// 名前と`assert`は、第34章のこの変化を確認するために書き換えてある
/// (元の第33章版の主張は、このテストの直前のドキュメントコメントに残した)。
#[test]
fn committed_data_survives_a_simulated_crash_via_recovery() {
    let path = temp_db_path("wal-crash-recovers-table-data");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)")
            .unwrap();
        db.flush().unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();
        db.execute("COMMIT").unwrap();
        // `db.flush()`を呼ばずにここでdropする。COMMIT直後にプロセスが
        // 死んだ状況を模している。
    }

    let mut db = Database::open(&path).unwrap();
    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(
        result.rows().len(),
        1,
        "第34章のRecoveryが、WALのInsert/CommitレコードからCOMMIT済みの行を再現しているはず"
    );
    assert_eq!(result.rows()[0].values(), &[Value::BigInt(100)]);

    remove_if_exists(&path);
    remove_if_exists(&wal_path(&path));
}

/// 上のテストと同じ「クラッシュ」を再現したうえで、今度はテーブル本体では
/// なくWALファイルを直接覗く。`COMMIT`がその応答を返す前にWALを同期している
/// ため、テーブル本体は復元できていなくても、コミットが実際に起きたという
/// 事実と、その行の内容(After Image)はディスク上に残っている。
///
/// この章はここまで(ログを先に書く)を実装する。ログを読んでテーブル本体を
/// 復元するRedoは第34章に譲るが、その第34章が読むべき材料がすでにここに
/// 揃っていることを、このテストが示す。
#[test]
fn the_commit_survives_the_same_simulated_crash_inside_the_wal_file() {
    let path = temp_db_path("wal-crash-keeps-the-log");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)")
            .unwrap();
        db.flush().unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();
        db.execute("COMMIT").unwrap();
        // ここでも`db.flush()`は呼ばない。
    }

    // `Database`をdropしたことで元の`WalWriter`(とそのファイルハンドル)も
    // 破棄されている。新しく`WalWriter::open`し直し、ファイルに実際に
    // 残っている内容だけを見る。
    let wal = minidb::WalWriter::open(wal_path(&path)).unwrap();
    let dump = wal.dump();
    assert!(
        dump.iter().any(|line| line.contains("type=Insert")),
        "Insertレコード(After Image)が残っているはず: {dump:?}"
    );
    assert!(
        dump.iter().any(|line| line.contains("type=Commit")),
        "Commitレコードが残っているはず: {dump:?}"
    );

    remove_if_exists(&path);
    remove_if_exists(&wal_path(&path));
}

/// `COMMIT`は、応答を返す前にWALを同期している。この順序を、`Database`を
/// dropした直後に(`flush`すら呼ばずに)WALファイルを開き直して確認する。
/// `execute("COMMIT")`がまだ`Ok`を返し切っていない段階でプロセスが死んだ
/// なら、この保証はそもそも意味を持たない。`execute`が`Ok`を返した**あとで**
/// dropしている、というテストコードの順序自体が主張の一部である。
#[test]
fn commit_syncs_the_wal_before_returning_success() {
    let path = temp_db_path("wal-commit-sync-order");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    db.flush().unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    let commit_result = db.execute("COMMIT");
    assert!(commit_result.is_ok());
    drop(db);

    let wal = minidb::WalWriter::open(wal_path(&path)).unwrap();
    assert!(wal.dump().iter().any(|line| line.contains("type=Commit")));

    remove_if_exists(&path);
    remove_if_exists(&wal_path(&path));
}

/// Autocommit(明示的な`BEGIN`を伴わない1文)も、それ自体が耐久性を持つ
/// 1本のトランザクションとしてWALへ書かれ、同期される。
#[test]
fn an_autocommit_statement_is_also_synced_before_it_returns() {
    let path = temp_db_path("wal-autocommit-sync");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
    db.flush().unwrap();

    db.execute("INSERT INTO t VALUES (1)").unwrap();
    drop(db);

    let wal = minidb::WalWriter::open(wal_path(&path)).unwrap();
    let dump = wal.dump();
    assert!(dump.iter().any(|line| line.contains("type=Insert")));
    assert!(dump.iter().any(|line| line.contains("type=Commit")));

    remove_if_exists(&path);
    remove_if_exists(&wal_path(&path));
}

/// `ROLLBACK`は、WALのBefore Imageを使って変更を打ち消す(第33章、
/// 第30章のメモリ上Undoの置き換え)。`Database`をまたぐ必要のない、
/// 同一プロセス内の通常のロールバックであることを確認する。
#[test]
fn rollback_restores_the_value_using_wal_before_images() {
    let path = temp_db_path("wal-rollback-restores");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)")
        .unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();
    db.flush().unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("UPDATE accounts SET balance = 999 WHERE id = 1").unwrap();
    db.execute("ROLLBACK").unwrap();

    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(result.rows()[0].values(), &[Value::BigInt(100)]);

    remove_if_exists(&path);
    remove_if_exists(&wal_path(&path));
}
