//! Benchmark(第40章)。
//!
//! criterionのような専用クレートは依存を増やすため採らず、`src/btree.rs`の
//! `lookup_time_grows_much_slower_than_table_size`(第24章)がすでに使っていた
//! 形、`Instant`で測って`println!`するだけの`#[ignore]`テストへ統一する。
//! 実行環境(CPU、ディスク、他プロセスの負荷)に左右される実測値そのものを
//! 数値目標として`assert`することはしない。ここでの役割は「回帰の目安」で
//! あり、章を書き足すたびにここへ来て手元で走らせ、直前の実行結果(このコード
//! と一緒にコミットはしない、手元のメモか`--nocapture`の出力をそのまま見る)
//! と比べて極端に遅くなっていないかを確認する運用を想定している。
//!
//! `cargo test --release --test bench -- --ignored --nocapture`で全件走る。
//! `--release`を付け忘れるとデバッグビルドの数値が支配的になり、実装同士の
//! 相対比較にならないので必ず付ける。

mod common;

use std::time::Instant;

use minidb::Database;

/// カタログは1ページに収まる範囲でしかテーブルの使用ページ一覧を持てない
/// (第15章、`DbError::CatalogTooLarge`)。VACUUM(第39章)を挟まずに
/// 挿入し続けるこのベンチマークは、1テーブルが専有できるページ数に上限が
/// あるぶん、行数もそれに応じて頭打ちにしてある。
const POINT_LOOKUP_SIZES: &[i64] = &[1_000, 5_000, 20_000];

/// このベンチマークのテーブルは、`id`・`v`の2列だけでなく`pad`列も持たせて
/// ある。列2つ・数百行を超えて増えるだけの細いテーブルでは、2枚目以降の
/// ページへ空きがあるにもかかわらずFree Space Mapが候補として拾えなくなり、
/// ページが際限なく増えて`DbError::CatalogTooLarge`へ突き当たる実装上の
/// 制約があることが、このベンチマークを実際に走らせる過程で見つかった
/// (原因の特定と修正は第15章の対象であり、この章の範囲を超えるため演習に
/// 残す)。列を1つ足すだけでこの制約を踏まずに済むため、ベンチマーク側で
/// 回避してある。
fn accounts_db(path: &std::path::Path) -> Database {
    let mut db = Database::open(path).unwrap();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, v BIGINT NOT NULL, pad BIGINT NOT NULL)").unwrap();
    db
}

fn remove_db(path: &std::path::Path) {
    let mut wal = path.as_os_str().to_os_string();
    wal.push(".wal");
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(&wal);
}

/// Micro Benchmark: Point Lookup。`id`に`PRIMARY KEY`があるので、第25章の
/// Index Scanが選ばれる経路を測る(`EXPLAIN`で確認済みの前提、本文参照)。
#[test]
#[ignore]
fn micro_point_lookup() {
    for &n in POINT_LOOKUP_SIZES {
        let path = common::temp_db_path(&format!("bench-point-lookup-{n}"));
        let mut db = accounts_db(&path);
        db.execute("BEGIN").unwrap();
        for i in 0..n {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {i}, 0)")).unwrap();
        }
        db.execute("COMMIT").unwrap();

        let start = Instant::now();
        for _ in 0..200 {
            db.execute(&format!("SELECT v FROM t WHERE id = {}", n - 1)).unwrap();
        }
        let elapsed = start.elapsed();
        println!("point_lookup n={n:>7} total={elapsed:?} avg={:?}", elapsed / 200);
        remove_db(&path);
    }
}

/// Micro Benchmark: Range Scan。全体の1%を`WHERE id BETWEEN`で読む。
#[test]
#[ignore]
fn micro_range_scan() {
    for &n in POINT_LOOKUP_SIZES {
        let path = common::temp_db_path(&format!("bench-range-scan-{n}"));
        let mut db = accounts_db(&path);
        db.execute("BEGIN").unwrap();
        for i in 0..n {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {i}, 0)")).unwrap();
        }
        db.execute("COMMIT").unwrap();

        let width = (n / 100).max(1);
        let start = Instant::now();
        for _ in 0..50 {
            db.execute(&format!("SELECT v FROM t WHERE id >= 0 AND id <= {width}")).unwrap();
        }
        let elapsed = start.elapsed();
        println!("range_scan   n={n:>7} width={width:>6} total={elapsed:?} avg={:?}", elapsed / 50);
        remove_db(&path);
    }
}

/// Micro Benchmark: Insert throughput。`BEGIN`/`COMMIT`で1本のトランザクションに
/// まとめた場合と、1行ごとにAutocommitした場合を並べて測る(WALの`sync`回数が
/// 支配的なコストであることを、本文の実測考察の材料にする)。
#[test]
#[ignore]
fn micro_insert_throughput() {
    for &n in &[1_000i64, 5_000] {
        let path = common::temp_db_path(&format!("bench-insert-batched-{n}"));
        let mut db = accounts_db(&path);
        let start = Instant::now();
        db.execute("BEGIN").unwrap();
        for i in 0..n {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {i}, 0)")).unwrap();
        }
        db.execute("COMMIT").unwrap();
        let elapsed = start.elapsed();
        println!("insert n={n:>7} mode=batched   total={elapsed:?} avg={:?}", elapsed / n as u32);
        remove_db(&path);

        let path = common::temp_db_path(&format!("bench-insert-autocommit-{n}"));
        let mut db = accounts_db(&path);
        let start = Instant::now();
        for i in 0..n {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {i}, 0)")).unwrap();
        }
        let elapsed = start.elapsed();
        println!("insert n={n:>7} mode=autocommit total={elapsed:?} avg={:?}", elapsed / n as u32);
        remove_db(&path);
    }
}

/// Join / Aggregate Benchmark: `customers` 500件と`orders` 8,000件を
/// Hash Joinで結合し、顧客ごとの合計金額を`GROUP BY`で集約する。
#[test]
#[ignore]
fn join_aggregate_benchmark() {
    let path = common::temp_db_path("bench-join-aggregate");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE customers (id BIGINT PRIMARY KEY, name TEXT NOT NULL)").unwrap();
    db.execute("CREATE TABLE orders (id BIGINT PRIMARY KEY, customer_id BIGINT NOT NULL, amount BIGINT NOT NULL)").unwrap();

    db.execute("BEGIN").unwrap();
    for i in 0..500i64 {
        db.execute(&format!("INSERT INTO customers VALUES ({i}, 'customer-{i}')")).unwrap();
    }
    for i in 0..8_000i64 {
        let customer_id = i % 500;
        let amount = 100 + (i % 500);
        db.execute(&format!("INSERT INTO orders VALUES ({i}, {customer_id}, {amount})")).unwrap();
    }
    db.execute("COMMIT").unwrap();

    let start = Instant::now();
    let result = db
        .execute(
            "SELECT customers.id, SUM(orders.amount) FROM customers \
             JOIN orders ON customers.id = orders.customer_id \
             GROUP BY customers.id",
        )
        .unwrap();
    let elapsed = start.elapsed();
    println!("join_aggregate customers=500 orders=8000 rows={} elapsed={elapsed:?}", result.rows().len());
    assert_eq!(result.rows().len(), 500, "全顧客が最低1件は注文を持つはず");

    remove_db(&path);
}

/// 小規模OLTP Benchmark: 口座間の送金(`BEGIN`・`UPDATE`×2・`COMMIT`)を
/// 直列に繰り返し、1トランザクションあたりの平均時間を測る。TPC-Bの
/// 送金1本ぶんを単一スレッドへ切り詰めた形(マルチスレッドでの計測は、
/// 第35章の実スレッドハーネスと組み合わせる発展課題として演習に残す)。
#[test]
#[ignore]
fn small_oltp_transfer_benchmark() {
    const ACCOUNTS: i64 = 100;
    const TRANSFERS: i64 = 2_000;

    let path = common::temp_db_path("bench-oltp-transfer");
    let mut db = accounts_db(&path);
    db.execute("BEGIN").unwrap();
    for i in 0..ACCOUNTS {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 10000, 0)")).unwrap();
    }
    db.execute("COMMIT").unwrap();

    let start = Instant::now();
    for i in 0..TRANSFERS {
        let from = i % ACCOUNTS;
        let to = (i + 1) % ACCOUNTS;
        db.execute("BEGIN").unwrap();
        db.execute(&format!("UPDATE t SET v = v - 1 WHERE id = {from}")).unwrap();
        db.execute(&format!("UPDATE t SET v = v + 1 WHERE id = {to}")).unwrap();
        db.execute("COMMIT").unwrap();
    }
    let elapsed = start.elapsed();
    println!(
        "oltp_transfer accounts={ACCOUNTS} transfers={TRANSFERS} total={elapsed:?} avg={:?} tps={:.0}",
        elapsed / TRANSFERS as u32,
        TRANSFERS as f64 / elapsed.as_secs_f64()
    );

    remove_db(&path);
}
