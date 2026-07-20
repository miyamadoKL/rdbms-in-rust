//! SQLレベルのProperty-based Test(第40章)。
//!
//! `src/btree.rs`のモデルテストは、ストレージ1層(B+Tree)がその仕様
//! (`BTreeMap`と同じ挙動)を満たすことを確認する。この章はもう1階層上、
//! SQL文を積み重ねたときに、どんな操作列でも崩れてはいけない不変条件を
//! 直接確認する。個々のSQL文が正しくても、束ねたときに崩れる不変条件が
//! あり得るため、Unit TestともGolden Testとも役割が違う。
//!
//! 2つの不変条件を確認する。
//!
//! - **行数の不変条件**: `INSERT`した行数と`DELETE`した行数の差は、常に
//!   `COUNT(*)`と一致する。
//! - **合計の不変条件**: 口座間の送金(ある行から引いた分だけ別の行へ足す)を
//!   何回繰り返しても、`SUM(balance)`は変わらない。`ROLLBACK`されたぶんは
//!   合計に反映されないことも同時に確認する。

mod common;

use common::Xorshift64;
use minidb::Value;

fn int_value(result: &minidb::QueryResult, row: usize, col: usize) -> i64 {
    match &result.rows()[row].values()[col] {
        Value::BigInt(n) => *n,
        other => panic!("BigIntを期待したが{other:?}が返った"),
    }
}

/// `INSERT`した行数と`DELETE`した行数の差が、常に`COUNT(*)`と一致する
/// (テーブル自身が主張する行数と、外から数えた増減が食い違わない)ことを、
/// シードを変えた50通りのランダムな操作列で確認する。
#[test]
fn row_count_matches_inserts_minus_deletes_for_random_operation_sequences() {
    for seed in 0..50u64 {
        let mut db = minidb::Database::memory();
        db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();

        let mut rng = Xorshift64::new(seed ^ 0xabcd_ef01_2345_6789);
        let mut alive: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
        let mut next_id = 0i64;

        for _ in 0..300 {
            if alive.is_empty() || rng.chance(2, 3) {
                let id = next_id;
                next_id += 1;
                db.execute(&format!("INSERT INTO t VALUES ({id}, {})", rng.range(1000))).unwrap();
                alive.insert(id);
            } else {
                let idx = rng.range(alive.len());
                let id = *alive.iter().nth(idx).unwrap();
                db.execute(&format!("DELETE FROM t WHERE id = {id}")).unwrap();
                alive.remove(&id);
            }

            let count = db.execute("SELECT COUNT(*) FROM t").unwrap();
            assert_eq!(int_value(&count, 0, 0), alive.len() as i64, "seed={seed}");
        }
    }
}

/// 口座間の送金を何度繰り返しても`SUM(balance)`が保存されることを、
/// シードを変えた30通りのランダムな送金列で確認する。3割の確率で
/// `ROLLBACK`し、その回だけは合計への反映が無いことも合わせて検証する。
#[test]
fn sum_of_balances_is_preserved_across_random_transfers_with_random_rollbacks() {
    const ACCOUNTS: i64 = 6;
    const INITIAL_BALANCE: i64 = 1_000;
    let total = ACCOUNTS * INITIAL_BALANCE;

    for seed in 0..30u64 {
        let mut db = minidb::Database::memory();
        db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
        for id in 0..ACCOUNTS {
            db.execute(&format!("INSERT INTO accounts VALUES ({id}, {INITIAL_BALANCE})")).unwrap();
        }

        let mut rng = Xorshift64::new(seed ^ 0x1357_9bdf_0246_8ace);
        // モデル: コミット済みの残高だけを反映するRust側の並行台帳。
        let mut model: Vec<i64> = vec![INITIAL_BALANCE; ACCOUNTS as usize];

        for _ in 0..40 {
            let from = rng.range(ACCOUNTS as usize) as i64;
            let mut to = rng.range(ACCOUNTS as usize) as i64;
            if to == from {
                to = (to + 1) % ACCOUNTS;
            }
            let amount = 1 + rng.range(50) as i64;
            let will_rollback = rng.chance(3, 10);

            db.execute("BEGIN").unwrap();
            db.execute(&format!("UPDATE accounts SET balance = balance - {amount} WHERE id = {from}")).unwrap();
            db.execute(&format!("UPDATE accounts SET balance = balance + {amount} WHERE id = {to}")).unwrap();
            if will_rollback {
                db.execute("ROLLBACK").unwrap();
            } else {
                db.execute("COMMIT").unwrap();
                model[from as usize] -= amount;
                model[to as usize] += amount;
            }

            let sum = db.execute("SELECT SUM(balance) FROM accounts").unwrap();
            assert_eq!(int_value(&sum, 0, 0), total, "seed={seed}: 送金の途中でも合計は保存されるはず");

            for id in 0..ACCOUNTS {
                let read = db.execute(&format!("SELECT balance FROM accounts WHERE id = {id}")).unwrap();
                assert_eq!(int_value(&read, 0, 0), model[id as usize], "seed={seed} id={id}");
            }
        }
    }
}
