//! Crash Recovery(第34章)のシナリオ別統合テスト。
//!
//! `Database`を`flush`せずに`drop`する(または`crate::failpoint`で途中に
//! 割り込ませる)ことで、実プロセスを殺さずに「この操作の直後にプロセスが
//! 死んだ」状況を再現し、そのあとで`Database::open`(内部で
//! `crate::recovery::recover`を呼ぶ)が正しい状態を復元することを確認する。

mod common;

use common::temp_db_path;
use minidb::{Database, Value, failpoint};

fn wal_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".wal");
    std::path::PathBuf::from(os_string)
}

fn remove_db(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal_path(path));
}

/// シナリオ(a): COMMITが応答を返した後・データページの書き戻し前にクラッシュ
/// しても、Redoが再現する。
#[test]
fn scenario_a_redo_restores_committed_rows_after_a_crash() {
    let path = temp_db_path("crash-scenario-a");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
        db.flush().unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();
        db.execute("INSERT INTO accounts VALUES (2, 200)").unwrap();
        db.execute("COMMIT").unwrap();
        // `db.flush()`を呼ばずにここでdropする。COMMIT直後にプロセスが
        // 死んだ状況を模している。
    }

    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert!(report.records_redone >= 2, "2件のInsertがRedoされているはず: {report:?}");
    assert_eq!(report.transactions_undone, 0, "このトランザクションはCommit済みでUndo不要のはず");

    let mut db = db;
    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(result.rows()[0].values(), &[Value::BigInt(100)]);
    let result = db.execute("SELECT balance FROM accounts WHERE id = 2").unwrap();
    assert_eq!(result.rows()[0].values(), &[Value::BigInt(200)]);

    remove_db(&path);
}

/// シナリオ(b): 未COMMITの変更が(`flush`によって)実際にページへ書かれた
/// 後にクラッシュしても、Undoがその変更を取り消す。
#[test]
fn scenario_b_undo_removes_an_uncommitted_change_already_written_to_the_page() {
    let path = temp_db_path("crash-scenario-b");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
        db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();
        db.flush().unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("UPDATE accounts SET balance = 999 WHERE id = 1").unwrap();
        db.execute("INSERT INTO accounts VALUES (2, 200)").unwrap();
        // ここでflushする。COMMIT前の変更が、実際にテーブル本体のページへ
        // 書き戻される(`BufferPool`のevictでも起こりうる状況を、明示的な
        // `flush`で確実に再現する)。
        db.flush().unwrap();
        // COMMIT・ROLLBACKのどちらも呼ばずにdropする。Activeなトランザクションの
        // 途中でプロセスが死んだ状況を模している。
    }

    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert_eq!(report.transactions_undone, 1, "Commit・Abortの無いトランザクションはUndoされるはず: {report:?}");

    let mut db = db;
    let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
    assert_eq!(result.rows()[0].values(), &[Value::BigInt(100)], "UPDATEはUndoで取り消されているはず");
    let result = db.execute("SELECT balance FROM accounts WHERE id = 2").unwrap();
    assert!(result.rows().is_empty(), "INSERTはUndoで取り消されているはず");

    remove_db(&path);
}

/// シナリオ(c): Undoの途中でRecoveryプロセス自身が(再度)クラッシュしても、
/// 次のRecoveryが最初からやり直して完走する。
///
/// `crate::failpoint`で、1つ目のloserトランザクションをUndoし終えた直後に
/// `Storage::open`(＝`recover`)自体を失敗させる。この章の設計
/// (`crate::recovery`モジュールドキュメントの「Undo中のクラッシュへの耐性」)
/// は、Analysis・Redo・Undoの全工程が終わるまでディスクへ一切書き戻さない
/// ことでこれに対応する。1回目の`Database::open`が失敗しても、ディスク上の
/// 状態は変化していないため、2回目の`Database::open`はまったく同じ入力から
/// Redo・Undoをやり直し、今度こそ両方のloserを取り消し切る。
#[test]
fn scenario_c_a_crash_during_undo_completes_on_the_next_recovery() {
    let path = temp_db_path("crash-scenario-c");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)").unwrap();
        db.flush().unwrap();

        // 決定的インターリーブテストハーネス(第30章)で、2本のトランザクションを
        // 同時にActiveにする(通常の`execute`経路はBEGINの入れ子を許さない)。
        let h1 = db.begin_tx();
        db.execute_in_tx(&h1, "INSERT INTO t VALUES (1, 100)").unwrap();
        let h2 = db.begin_tx();
        db.execute_in_tx(&h2, "INSERT INTO t VALUES (2, 200)").unwrap();

        // 別の(Autocommitの)1文を実行し、WALのsyncを1回発生させる。syncは
        // バッファ全体をディスクへ書き渡すため、h1・h2がまだ書いた
        // Insertレコード(コミットしていない)も、この時点でディスクへ届く。
        db.execute("INSERT INTO t VALUES (3, 300)").unwrap();

        // h1・h2のどちらもcommit_tx・rollback_txを呼ばずにdropする。
        // 2本のActiveなトランザクションを残したままクラッシュした状況を模す。
    }

    failpoint::arm("recovery_undo_step", 1);
    assert!(Database::open(&path).is_err(), "1つ目のloserをUndoした直後に失敗するよう仕込んだ");

    // 2回目のOpenは同じ場所では失敗しない(failpointは1回発火すると
    // 自動でdisarmされる、`crate::failpoint`を参照)。
    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert_eq!(report.transactions_undone, 2, "2本のloserがどちらもUndoされているはず: {report:?}");

    let mut db = db;
    assert!(db.execute("SELECT v FROM t WHERE id = 1").unwrap().rows().is_empty());
    assert!(db.execute("SELECT v FROM t WHERE id = 2").unwrap().rows().is_empty());
    let committed = db.execute("SELECT v FROM t WHERE id = 3").unwrap();
    assert_eq!(committed.rows()[0].values(), &[Value::BigInt(300)], "Autocommitの行はそのまま残っているはず");

    remove_db(&path);
}

/// シナリオ(d): `CHECKPOINT`の直後にクラッシュすると、AnalysisはCheckpoint
/// より前を走査せずに済む。
#[test]
fn scenario_d_checkpoint_shortens_the_analysis_scan() {
    let path = temp_db_path("crash-scenario-d");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        db.flush().unwrap();

        for i in 0..30i64 {
            db.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        db.execute("CHECKPOINT").unwrap();
        db.execute("INSERT INTO t VALUES (999)").unwrap();
        // CHECKPOINT以降に1文だけ実行してdropする。
    }

    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert!(report.used_checkpoint, "Checkpointレコードが見つかっているはず: {report:?}");
    // Checkpoint以降のレコードはBegin・Insert・Commitの3件だけのはず。
    assert!(report.records_scanned <= 5, "Checkpointより前を読み飛ばしているはず: {report:?}");

    let mut db = db;
    let result = db.execute("SELECT id FROM t WHERE id = 999").unwrap();
    assert_eq!(result.rows().len(), 1);
    let result = db.execute("SELECT id FROM t WHERE id = 0").unwrap();
    assert_eq!(result.rows().len(), 1, "Checkpoint以前にCommit済みの行も引き続き読めるはず");

    remove_db(&path);
}

/// シナリオ(e): 1件も書き込んでいないActiveなトランザクションを含む
/// `CHECKPOINT`のあとでクラッシュしても、次のRecoveryはpanicせずに完走する。
///
/// `BEGIN`だけではまだWALレコードを書かない(`WalWriter::ensure_begin`が
/// 実際の書き込みが起きるまで`Begin`レコードの発行を遅らせる、`src/wal.rs`
/// を参照)。したがって、この状態で`CHECKPOINT`を迎えたトランザクションの
/// Active Transaction一覧上の`last_lsn`は`None`のままである。Analysisは
/// このトランザクションを「Undo対象だが起点となるLSNを持たない」トランザク
/// ションとして扱わなければならず、架空の`Lsn(0)`を作って`WalWriter::record`
/// に渡すとその`Lsn`はWAL上に存在しないため`panic`する(この章のレビューで
/// 実際に指摘された不具合)。
#[test]
fn scenario_e_checkpoint_with_an_active_transaction_that_has_not_written_anything_yet() {
    let path = temp_db_path("crash-scenario-e");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        db.flush().unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("CHECKPOINT").unwrap();
        // COMMIT・ROLLBACKのどちらも呼ばずにdropする。CHECKPOINTの時点で
        // このトランザクションはまだ1件もWALレコードを書いていない。
    }

    // 修正前はここで`recovery.rs`の`apply_wal_undo_disk`がLsn(0)を参照して
    // panicしていた。
    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert!(report.used_checkpoint, "Checkpointレコードが見つかっているはず: {report:?}");
    assert_eq!(report.transactions_undone, 1, "書き込みの無いActiveなトランザクションもUndo対象として解決されるはず");

    let mut db = db;
    assert!(db.execute("SELECT id FROM t").unwrap().rows().is_empty());
    remove_db(&path);
}

/// シナリオ(f): `CHECKPOINT`の時点ではまだ何も書いていなかったトランザク
/// ションが、`CHECKPOINT`の**あとで**書き込みを始めてからcrashしても、
/// Undoが正しくその書き込みだけを取り消す(境界値: `last_lsn`が`None`から
/// `Some`へ切り替わる瞬間をまたぐ)。
#[test]
fn scenario_f_a_transaction_starts_writing_only_after_the_checkpoint_it_was_active_at() {
    let path = temp_db_path("crash-scenario-f");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        db.flush().unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("CHECKPOINT").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        // COMMIT・ROLLBACKを呼ばずにdropする。
    }

    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert_eq!(report.transactions_undone, 1, "Checkpoint後に始まった書き込みもUndoされるはず: {report:?}");

    let mut db = db;
    assert!(db.execute("SELECT id FROM t").unwrap().rows().is_empty(), "未確定のINSERTはUndoで取り消されているはず");
    remove_db(&path);
}

/// シナリオ(g): 決定的インターリーブテストハーネス(`begin_tx`)で開始した
/// トランザクションも、`CHECKPOINT`のActive Transaction Tableに含まれる。
///
/// `execute_checkpoint`が通常のSQL経路の`self.tx`だけを見て
/// `harness_contexts`を無視していると、ここで`begin_tx`したトランザクション
/// の存在をAnalysisが知らないまま`CHECKPOINT`をまたぎ、未確定の`INSERT`が
/// Undoされずに残ってしまう(この章のレビューで実際に指摘された不具合)。
#[test]
fn scenario_g_a_harness_transaction_active_at_checkpoint_time_is_also_undone() {
    let path = temp_db_path("crash-scenario-g");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        db.flush().unwrap();

        let handle = db.begin_tx();
        db.execute_in_tx(&handle, "INSERT INTO t VALUES (1)").unwrap();
        db.execute("CHECKPOINT").unwrap();
        // `handle`はcommit_tx・rollback_txを呼ばずにdropする。
    }

    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert!(report.used_checkpoint, "Checkpointレコードが見つかっているはず: {report:?}");
    assert_eq!(report.transactions_undone, 1, "ハーネスのActiveなトランザクションもUndo対象に含まれるはず: {report:?}");

    let mut db = db;
    assert!(
        db.execute("SELECT id FROM t").unwrap().rows().is_empty(),
        "ハーネスのトランザクションが未確定のまま残した行はUndoで取り消されているはず"
    );
    remove_db(&path);
}

/// `db_path`の索引`index_name`が使う専用ファイルのパス(`src/storage.rs`の
/// `index_file_path`と同じ命名規則、テストからは非公開関数を呼べないため
/// ここで複製する)。
fn index_file_path(db_path: &std::path::Path, index_name: &str) -> std::path::PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".idx.");
    os_string.push(index_name);
    std::path::PathBuf::from(os_string)
}

/// シナリオ(h)・(i): 索引ありのRedo・Undo失敗それぞれのfailpointについて、
/// 「失敗後に索引ファイルのバイト列が不変」かつ「次回Openが完走する」ことを
/// 確認する。
///
/// 索引の作り直しは、Redoの前(`recovery_redo_step`より前)に一時ファイルへ
/// 行われ、Redo・Undoの全工程が終わった`Storage::flush`・`sync`の直後に
/// `rename`で本来の索引ファイルへ置き換わる(`Storage::
/// rebuild_all_indexes_after_recovery`のドキュメントを参照)。したがって、
/// Redoの途中(`recovery_redo_step`)・Undoの途中(`recovery_undo_step`)の
/// どちらでRecoveryが失敗しても、`rename`にはまだ到達しておらず、既存の
/// 索引ファイルのバイト列は`Database::open`を呼ぶ直前と一切変わらないはず
/// である。
#[test]
fn scenario_h_and_i_index_files_are_unchanged_when_recovery_fails_at_redo_or_undo() {
    for failpoint_name in ["recovery_redo_step", "recovery_undo_step"] {
        let path = temp_db_path(&format!("crash-scenario-index-failpoint-{failpoint_name}"));
        {
            let mut db = Database::open(&path).unwrap();
            db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)").unwrap();
            db.execute("CREATE INDEX idx_v ON t (v)").unwrap();
            db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
            db.flush().unwrap();

            // 未確定のトランザクションを残したままdropする。Undoの対象を
            // 必ず1本作っておくことで、`recovery_undo_step`のfailpointが
            // 確実に発火する状況を用意する。決定的インターリーブテスト
            // ハーネス(`begin_tx`)を使うのは、通常の`execute`経路の`BEGIN`
            // だと、後続のすべての文が(Autocommitではなく)同じ未確定
            // トランザクションの一部になってしまい、下のWAL sync用の1文が
            // 狙いどおりAutocommitにならないため(`scenario_c`と同じ理由)。
            let h = db.begin_tx();
            db.execute_in_tx(&h, "INSERT INTO t VALUES (2, 20)").unwrap();
            db.execute_in_tx(&h, "UPDATE t SET v = 999 WHERE id = 1").unwrap();

            // `h`が積んだWALレコードは、まだ`WalWriter`のメモリ上のバッファに
            // しか無い。別の(Autocommitの)1文を実行してWALのsyncを1回
            // 発生させ、上の`h`のレコードも確実にディスクへ届かせる
            // (`scenario_c`と同じ理由)。
            db.execute("INSERT INTO t VALUES (3, 30)").unwrap();
        }

        let index_path = index_file_path(&path, "idx_v");
        let bytes_before = std::fs::read(&index_path).unwrap();

        failpoint::arm(failpoint_name, 1);
        assert!(Database::open(&path).is_err(), "{failpoint_name}の直後に失敗するよう仕込んだ");

        let bytes_after_failure = std::fs::read(&index_path).unwrap();
        assert_eq!(bytes_before, bytes_after_failure, "{failpoint_name}での失敗後も索引ファイルのバイト列は不変のはず");

        // 2回目のOpenはfailpointが自動でdisarmされているため完走する。
        let mut db = Database::open(&path).unwrap();
        let result = db.execute("SELECT id FROM t WHERE v = 999").unwrap();
        assert!(result.rows().is_empty(), "未確定のUPDATEはUndoされているはず");
        let result = db.execute("SELECT id FROM t WHERE v = 10").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(1)], "id=1のvは999にUPDATEされ、Undoで元の10へ戻っているはず");
        let result = db.execute("SELECT v FROM t WHERE id = 1").unwrap();
        assert_eq!(result.rows()[0].values(), &[Value::BigInt(10)]);
        assert!(db.execute("SELECT id FROM t WHERE v = 20").unwrap().rows().is_empty(), "未確定のINSERTもUndoされているはず");

        remove_db(&path);
    }
}

/// Recoveryは冪等である: 一度Recoveryが完了した直後にもう一度開き直しても、
/// 何もRedo・Undoすることが残っていない。
#[test]
fn recovery_is_idempotent_on_a_second_open_right_after_the_first() {
    let path = temp_db_path("crash-scenario-idempotent");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY)").unwrap();
        db.flush().unwrap();
        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (1)").unwrap();
        db.execute("COMMIT").unwrap();
    }

    {
        let db = Database::open(&path).unwrap();
        let report = db.last_recovery_report().unwrap();
        assert!(report.records_redone >= 1);
    }

    // 直前の`Database::open`(Recovery込み)がすでにflush・syncまで終えて
    // いるので、続けてもう一度開いても今度はRedo・Undoの対象が無い。
    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert_eq!(report.records_redone, 0, "直前のRecoveryが完全に反映済みのはず: {report:?}");
    assert_eq!(report.transactions_undone, 0);

    remove_db(&path);
}

/// クラッシュ後も索引は正しく作り直される(Heapから丸ごと再構築、第34章)。
#[test]
fn indexes_are_rebuilt_correctly_after_a_crash() {
    let path = temp_db_path("crash-scenario-index-rebuild");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT)").unwrap();
        db.execute("CREATE INDEX idx_v ON t (v)").unwrap();
        db.flush().unwrap();

        db.execute("BEGIN").unwrap();
        db.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        db.execute("INSERT INTO t VALUES (2, 20)").unwrap();
        db.execute("COMMIT").unwrap();
        // flushを呼ばずにdropする。
    }

    let mut db = Database::open(&path).unwrap();
    // 索引が壊れたまま(Heapと食い違ったまま)残っていないことを、
    // 索引経由でも一致検索できることで確認する。行数が少なくコストベースの
    // 選択(第28章)がSeqScanを選ぶ場合もあるため、ここではEXPLAINでの
    // アクセスパスではなく検索結果そのものを確認する。
    let result = db.execute("SELECT id FROM t WHERE v = 20").unwrap();
    assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
    // `CREATE INDEX`と同じ索引名の再作成を試みるとエラーになる(作り直された
    // 索引がカタログに正しく1本だけ登録されている証拠)。
    assert!(db.execute("CREATE INDEX idx_v ON t (v)").is_err());

    let _ = std::fs::remove_file(format!("{}.idx.idx_v", path.display()));
    remove_db(&path);
}
