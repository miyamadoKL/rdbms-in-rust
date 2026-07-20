//! Crash Injectionの統合(第40章)。
//!
//! `tests/crash_recovery.rs`のシナリオ(c)は、`crate::failpoint`で「1つ目の
//! loserトランザクションをUndoし終えた直後」という**1つの決まった位置**に
//! Recovery自身を失敗させ、2回目の`Database::open`が最初からやり直して
//! 完走することを確認していた。この章はその位置を固定せず、ワークロードも
//! クラッシュ位置もシードから決定的に選び直す形へ一般化する。
//!
//! 1回のイテレーションは次の手順を踏む。
//!
//! 1. シードからランダムな送金ワークロード(コミット済みの送金を何本かと、
//!    最後に1本だけ残す未コミットの送金)を組み立て、`flush`しないまま
//!    `Database`を`drop`してクラッシュを模す。
//! 2. 同じシードでもう一度同じワークロードを別のファイルへ組み立て、今度は
//!    `crate::failpoint`で`recovery_redo_step`・`recovery_undo_step`の
//!    どちらか一方に、実際に踏まれる回数の範囲内でランダムな発火位置を仕込む。
//! 3. 1回目の`Database::open`はその位置で失敗することを確認する。
//! 4. 2回目の`Database::open`(failpointは発火すると自動でdisarmされる)が
//!    完走し、コミット済みの送金だけが反映され、未コミットの送金は跡形もなく
//!    Undoされていて、口座の合計金額(送金では変わらないはずの不変条件)が
//!    保存されていることを確認する。
//!
//! 同じワークロードを2回組み立て直しているのは、`Database::open`が
//! Recoveryに成功すると`flush`・`sync`・索引の`rename`でディスク上の状態を
//! 書き換えてしまい、1回目に使ったファイルではもう「クラッシュ直後」の
//! バイト列を再現できないためである。

mod common;

use std::collections::HashMap;

use common::{Xorshift64, temp_db_path};
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

const ACCOUNTS: i64 = 5;
const INITIAL_BALANCE: i64 = 1_000;
const COMMITTED_TRANSFERS: usize = 6;

/// `seed`から決定的に送金ワークロードを`path`へ組み立て、`flush`しないまま
/// `Database`をスコープの外へ出してクラッシュを模す。戻り値は、コミット済みの
/// 送金だけを反映した期待残高。
fn build_workload(path: &std::path::Path, seed: u64) -> HashMap<i64, i64> {
    let mut rng = Xorshift64::new(seed);
    let mut expected: HashMap<i64, i64> = (0..ACCOUNTS).map(|id| (id, INITIAL_BALANCE)).collect();

    {
        let mut db = Database::open(path).unwrap();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
        for id in 0..ACCOUNTS {
            db.execute(&format!("INSERT INTO accounts VALUES ({id}, {INITIAL_BALANCE})")).unwrap();
        }
        db.flush().unwrap();

        for i in 0..COMMITTED_TRANSFERS {
            let from = rng.range(ACCOUNTS as usize) as i64;
            let to = (from + 1 + rng.range((ACCOUNTS - 1) as usize) as i64) % ACCOUNTS;
            let amount = 1 + rng.range(30) as i64;
            db.execute("BEGIN").unwrap();
            db.execute(&format!("UPDATE accounts SET balance = balance - {amount} WHERE id = {from}")).unwrap();
            db.execute(&format!("UPDATE accounts SET balance = balance + {amount} WHERE id = {to}")).unwrap();
            db.execute("COMMIT").unwrap();
            *expected.get_mut(&from).unwrap() -= amount;
            *expected.get_mut(&to).unwrap() += amount;
            // 半分は明示的にflushし、半分はWALだけに残す。Redoが再現すべき量と
            // すでにページへ届いている量の両方をワークロードへ混ぜるため。
            if i % 2 == 0 {
                db.flush().unwrap();
            }
        }

        // 最後の1本はCOMMITせずにdropする。これが唯一のloserトランザクション
        // になる(`expected`には反映しない)。
        let from = rng.range(ACCOUNTS as usize) as i64;
        let to = (from + 1) % ACCOUNTS;
        db.execute("BEGIN").unwrap();
        db.execute(&format!("UPDATE accounts SET balance = balance - 999999 WHERE id = {from}")).unwrap();
        db.execute(&format!("UPDATE accounts SET balance = balance + 999999 WHERE id = {to}")).unwrap();
        // WALのバッファはCOMMIT・`flush`のどちらかを呼ぶまでファイルへ届かない
        // (`src/wal.rs`の`WalWriter::flush`を参照)。ここで`flush`してWALと
        // ページの両方をディスクへ押し出してから、COMMIT・ROLLBACKのどちらも
        // 呼ばずにdropし、Activeなトランザクションの途中でクラッシュした
        // 状況を作る。
        db.flush().unwrap();
    }

    expected
}

fn total(expected: &HashMap<i64, i64>) -> i64 {
    expected.values().sum()
}

/// `dry_run_path`で一度Recoveryを完走させ、`records_scanned`・
/// `transactions_undone`の実測値を得る。この値をもとに、armする側の
/// ワークロードで実際に意味のある発火位置(範囲内)を選べる。
fn measure_recovery_bounds(dry_run_path: &std::path::Path) -> (usize, usize) {
    let db = Database::open(dry_run_path).unwrap();
    let report = db.last_recovery_report().unwrap();
    (report.records_scanned, report.transactions_undone)
}

/// シードを変えた12通りのワークロードそれぞれについて、Redo・Undoいずれかの
/// ランダムな位置でRecoveryを1回失敗させ、2回目のRecoveryが完全に復元する
/// ことを確認する。
#[test]
fn recovery_survives_a_crash_at_a_random_redo_or_undo_position() {
    for seed in 0..12u64 {
        let dry_run_path = temp_db_path(&format!("crash-loop-dry-{seed}"));
        let expected = build_workload(&dry_run_path, seed);
        let (records_scanned, transactions_undone) = measure_recovery_bounds(&dry_run_path);
        assert_eq!(transactions_undone, 1, "seed={seed}: loserは常に1本のはず");
        assert!(records_scanned > 0, "seed={seed}: Redo対象が無いとテストの意味が無い");
        remove_db(&dry_run_path);

        let mut rng = Xorshift64::new(seed ^ 0x2222_1111_0000_ffff);
        let armed_path = temp_db_path(&format!("crash-loop-armed-{seed}"));
        build_workload(&armed_path, seed);

        // 半々でRedo・Undoのどちらかを狙う。Undo側は必ず1本しか無いので
        // 発火位置は常に1固定、Redo側は実測した範囲からランダムに選ぶ。
        let (failpoint_name, count) = if rng.chance(1, 2) {
            ("recovery_redo_step", 1 + rng.range(records_scanned))
        } else {
            ("recovery_undo_step", 1usize)
        };

        failpoint::arm(failpoint_name, count);
        assert!(Database::open(&armed_path).is_err(), "seed={seed} {failpoint_name}の{count}回目で失敗するよう仕込んだ");

        // 2回目のOpenはfailpointが自動でdisarmされているため完走する。
        let db = Database::open(&armed_path).unwrap();
        let report = db.last_recovery_report().unwrap();
        assert_eq!(report.transactions_undone, 1, "seed={seed}: 2回目もloserを1本Undoするはず: {report:?}");

        let mut db = db;
        let mut actual_total = 0i64;
        for id in 0..ACCOUNTS {
            let read = db.execute(&format!("SELECT balance FROM accounts WHERE id = {id}")).unwrap();
            let balance = match read.rows()[0].values()[0] {
                Value::BigInt(n) => n,
                ref other => panic!("BigIntを期待したが{other:?}が返った"),
            };
            assert_eq!(balance, expected[&id], "seed={seed} id={id}: {failpoint_name}の位置{count}で失敗させた後の復元結果が食い違う");
            actual_total += balance;
        }
        assert_eq!(actual_total, total(&expected), "seed={seed}: 送金は合計を変えないはずの不変条件");

        remove_db(&armed_path);
    }
}

/// 上のテストの重い版(手元での再現用)。`cargo test --release --test
/// crash_loop -- --ignored recovery_survives_a_crash_at_every_redo_position
/// --nocapture`で実行する。ランダムな1点ではなく、Redoの**全ステップ**を
/// 1つずつ狙って失敗させ、どの位置で切っても復元できることを網羅する。
#[test]
#[ignore]
fn recovery_survives_a_crash_at_every_redo_position() {
    for seed in 0..5u64 {
        let dry_run_path = temp_db_path(&format!("crash-loop-exhaustive-dry-{seed}"));
        let expected = build_workload(&dry_run_path, seed);
        let (records_scanned, _) = measure_recovery_bounds(&dry_run_path);
        remove_db(&dry_run_path);

        for count in 1..=records_scanned {
            let armed_path = temp_db_path(&format!("crash-loop-exhaustive-armed-{seed}-{count}"));
            build_workload(&armed_path, seed);
            failpoint::arm("recovery_redo_step", count);
            assert!(Database::open(&armed_path).is_err(), "seed={seed} count={count}");

            let mut db = Database::open(&armed_path).unwrap();
            for id in 0..ACCOUNTS {
                let read = db.execute(&format!("SELECT balance FROM accounts WHERE id = {id}")).unwrap();
                let balance = match read.rows()[0].values()[0] {
                    Value::BigInt(n) => n,
                    ref other => panic!("BigIntを期待したが{other:?}が返った"),
                };
                assert_eq!(balance, expected[&id], "seed={seed} count={count} id={id}");
            }
            remove_db(&armed_path);
        }
    }
}
