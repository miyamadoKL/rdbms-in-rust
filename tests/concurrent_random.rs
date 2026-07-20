//! 決定的ランダムインターリーブ(第40章)。
//!
//! `tests/interleave.rs`・`tests/interleave_disk.rs`(第30〜32章)は、2本の
//! トランザクションの操作順を著者が手で1通り選び、その1通りについて
//! 「異常が起きる/起きない」を確認してきた。この章はその手作業を、シードから
//! 決定的に選んだ**ランダムな操作順**へ置き換える。狙いは異常の有無ではなく、
//! Strict 2PL(第31章)が主張する性質そのもの、「実際にどう入り組んで実行
//! されても、結果はどれかの直列実行と一致する(直列化可能性)」を、たくさんの
//! 順序で踏んでも崩れないことである。
//!
//! 2本のトランザクションT1・T2が、2行(id=1、id=2)の`tag`列に自分の名前を
//! 書き込む。書き込み先の行の順序はシードごとにランダムに変え、どちらを
//! 先に試すかもランダムなスケジューラが決める。上書きなので、最後にどちらが
//! 書いたかで結果が変わる非可換な操作であり、書いた順序を混同すると
//! すぐに矛盾した結果(2行が別々のトランザクションの名前を持つ)として
//! 現れる。
//!
//! Strict 2PLのもとでは、競合する2本のトランザクションのうち後から
//! `COMMIT`が通った方が、実際の実行としても「後に直列実行された」ことに
//! なる(先にCOMMITした方が持っていたロックを、後にCOMMITする方が
//! `COMMIT`の直前まで待たされてから引き継ぐほかないため)。したがって
//! 「両方の行が、実際に最後にCOMMITしたトランザクションの名前を持つ」ことを
//! 確認すれば、直列化可能性を壊す実行が無かったと言える。
//!
//! デッドロックはVictim Selection(第32章)で自動的に解消されるので、
//! Victimになった側をこのテストのスケジューラがその場で`ROLLBACK`して
//! 最初から(同じ行順序で)やり直す。

mod common;

use common::{Xorshift64, temp_db_path};
use minidb::error::DbError;
use minidb::{Database, TxHandle, Value};

fn text_value(result: &minidb::QueryResult, row: usize, col: usize) -> String {
    match &result.rows()[row].values()[col] {
        Value::Text(s) => s.clone(),
        other => panic!("Textを期待したが{other:?}が返った"),
    }
}

/// 1本のトランザクションの進行状況。`row_order`はこのトランザクションが
/// `UPDATE`する行idの順序で、生成時に固定する(デッドロックで再試行しても
/// 同じ順序で最初からやり直す)。
struct Runner {
    name: &'static str,
    row_order: [i64; 2],
    handle: TxHandle,
    pos: usize,
}

impl Runner {
    fn start(db: &mut Database, name: &'static str, row_order: [i64; 2]) -> Self {
        Runner { name, row_order, handle: db.begin_tx(), pos: 0 }
    }

    fn next_sql(&self) -> String {
        format!("UPDATE rows_t SET tag = '{}' WHERE id = {}", self.name, self.row_order[self.pos])
    }

    fn done(&self) -> bool {
        self.pos == self.row_order.len()
    }
}

/// シードを変えた60通りの行順序・スケジュール順で、最終的に2行が必ず
/// 同じトランザクション(実際に最後にCOMMITした方)の名前を持つことを
/// 確認する。
#[test]
fn random_interleaving_always_matches_the_actual_commit_order() {
    for seed in 0..60u64 {
        let mut rng = Xorshift64::new(seed ^ 0x0ff1_ce0f_f1ce_0000);
        let path = temp_db_path(&format!("concurrent-random-{seed}"));
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE rows_t (id BIGINT PRIMARY KEY, tag TEXT NOT NULL)").unwrap();
        db.execute("INSERT INTO rows_t VALUES (1, 'init')").unwrap();
        db.execute("INSERT INTO rows_t VALUES (2, 'init')").unwrap();

        let order_for = |rng: &mut Xorshift64| if rng.chance(1, 2) { [1, 2] } else { [2, 1] };
        let mut t1 = Runner::start(&mut db, "T1", order_for(&mut rng));
        let mut t2 = Runner::start(&mut db, "T2", order_for(&mut rng));
        let mut commit_order: Vec<&'static str> = Vec::new();

        let mut ticks = 0usize;
        while commit_order.len() < 2 {
            ticks += 1;
            assert!(ticks < 10_000, "seed={seed}: スケジューラが進行しなくなった(ライブロック)");

            // 未完了のRunnerをランダムに1本選び、その次の1文だけ実行する。
            let pick_t1 = if t1.done() {
                false
            } else if t2.done() {
                true
            } else {
                rng.chance(1, 2)
            };
            let runner = if pick_t1 { &mut t1 } else { &mut t2 };
            if runner.done() {
                continue;
            }

            match db.execute_in_tx(&runner.handle, &runner.next_sql()) {
                Ok(_) => {
                    runner.pos += 1;
                    if runner.done() {
                        db.commit_tx(runner.handle).unwrap();
                        commit_order.push(runner.name);
                    }
                }
                Err(DbError::WouldBlock) => {
                    // ロックが空くまで、この文はまだ実行されていない。
                    // 次のtickで別のRunnerが先に進むのを待つ。
                }
                Err(DbError::DeadlockDetected) => {
                    // Victimになった側は、同じ行順序で最初からやり直す。
                    db.rollback_tx(runner.handle).ok();
                    let name = runner.name;
                    let row_order = runner.row_order;
                    *runner = Runner::start(&mut db, name, row_order);
                }
                Err(other) => panic!("seed={seed}: 想定外のエラー: {other:?}"),
            }
        }

        assert_eq!(commit_order.len(), 2, "seed={seed}: 2本ともCOMMITしているはず");
        let winner = commit_order[1];
        let tag1 = text_value(&db.execute("SELECT tag FROM rows_t WHERE id = 1").unwrap(), 0, 0);
        let tag2 = text_value(&db.execute("SELECT tag FROM rows_t WHERE id = 2").unwrap(), 0, 0);
        assert_eq!(tag1, winner, "seed={seed}: 実際に最後にCOMMITした{winner}の名前と食い違う(直列化可能性違反)");
        assert_eq!(tag2, winner, "seed={seed}: 実際に最後にCOMMITした{winner}の名前と食い違う(直列化可能性違反)");

        drop(db);
        let mut wal_path = path.as_os_str().to_os_string();
        wal_path.push(".wal");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&wal_path);
    }
}
