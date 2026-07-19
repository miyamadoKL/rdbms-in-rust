//! 実スレッドの並行テスト(第35章)。
//!
//! 第30〜34章の並行実行テストは、すべて単一スレッド上の決定的ハーネス
//! (`Database::begin_tx`・`execute_in_tx`を手動でインターリーブさせる方式)
//! で行ってきた。この章はBuffer PoolとB+Treeを本物のLatch(`RwLock`)で
//! スレッドセーフにし、`Database`を`SharedDatabase`で包んだことで、初めて
//! `std::thread::spawn`した本物のOSスレッドから同じデータ構造を触るテストを
//! 書けるようになった。
//!
//! どのテストも、検証は**タイミングに依存しない決定的な最終状態**
//! (全キーがlookupできる、合計金額が保存されている等)だけを見る。スレッドの
//! 実行順序そのものをアサートするテストは1つも無い。

mod common;

use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use minidb::{BTree, BufferPool, DataType, DbError, DiskManager, PageId, RecordId, SharedDatabase, SlotId, Value};

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let unique = format!(
        "minidb-concurrent-threads-test-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_nanos()
    );
    path.push(unique);
    path
}

fn open_btree(path: &std::path::Path) -> BTree {
    let disk = DiskManager::open(path).unwrap();
    BTree::create(BufferPool::new(disk, 64), DataType::BigInt, true).unwrap()
}

fn dummy_rid() -> RecordId {
    RecordId::new(PageId(1), SlotId(0))
}

/// 複数スレッドが同じ`Arc<BTree>`へ、互いに素なキー集合を同時に`insert`する。
///
/// `THREADS`本のスレッドがそれぞれ`PER_THREAD`件、重ならない範囲のキーを
/// 挿入する。`Barrier`で全スレッドの開始を揃えるため、木がまだ浅い
/// (Root 1枚だけの)段階から並行アクセスが集中し、Leaf Split・Root Split
/// の最中に他スレッドが同じ経路へ踏み込む場面を高い確率で作る。
///
/// 検証は「全スレッドが挿入したキーが、すべて`lookup`で見つかる」という
/// 最終状態だけであり、どのスレッドがどの順で分割を起こしたかには一切
/// 依存しない。
#[test]
fn concurrent_inserts_from_multiple_threads_are_all_findable() {
    const THREADS: i64 = 8;
    const PER_THREAD: i64 = 400;

    let path = temp_path("btree-concurrent-insert");
    let btree = Arc::new(open_btree(&path));
    let barrier = Arc::new(Barrier::new(THREADS as usize));

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let btree = Arc::clone(&btree);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..PER_THREAD {
                    let key = t * PER_THREAD + i;
                    btree.insert(&Value::BigInt(key), dummy_rid()).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    for t in 0..THREADS {
        for i in 0..PER_THREAD {
            let key = t * PER_THREAD + i;
            let found = btree.lookup(&Value::BigInt(key)).unwrap();
            assert_eq!(found.len(), 1, "key={key}が見つからない、または重複している");
        }
    }
    // 分割が実際に起きて、木が単一のLeaf Pageのままではなくなっていることも
    // あわせて確認する(並行アクセスがSplitを一切経由しなかった場合、
    // このテストはLock Couplingを何も検証していないことになる)。
    assert!(btree.height().unwrap() > 1, "十分な件数を挿入したのにRoot Splitが一度も起きていない");

    std::fs::remove_file(&path).unwrap();
}

/// 複数スレッドが`Arc<BTree>`へ同時に`insert`しつつ、別のスレッドが並行して
/// `range`(全件走査)を呼び続けても壊れない(Read LatchとWrite Latchの共存)。
///
/// # Readerに課す条件を「クラッシュしない」までに絞る理由
///
/// `RangeScan`(`crate::btree`)は、`Iterator::next`の呼び出しの**外側**で
/// Latchを持ち越さない設計を選んでいる(`next`から`next`への間、つまり
/// 呼び出し元がイテレータの結果をどう使おうと、その間ずっとページを
/// Latchし続けるのは、ロックをユーザーの考え中ずっと握るのと同じくらい
/// 具合が悪い)。この設計の代償として、走査の**途中**で他スレッドの`insert`
/// によるSplitが割り込むと、走査中のスレッドは、次の葉に移った時点までに
/// 起きたSplitの結果を、途中から混ぜて観測することがある。個々の
/// `next()`呼び出しはそれぞれ正しいLatchの下で行われるため、壊れた
/// (`CorruptPage`)ページを読んだり、`panic`したりすることは無い。しかし、
/// 走査全体を通した「常に昇順」という性質までは、この設計では保証しない
/// (Latchを葉から葉へ渡す前に外へ手放すRange Scanの限界。この限界を
/// 解消するBLinkTree等の設計は演習問題とする)。
///
/// そのため、Readerスレッドに課す条件は「Writerが挿入を終えるまでの間、
/// 走査がエラーにならず完走し続けること」だけに絞る。**昇順であることの
/// 検証は、全スレッドが合流し、書き込みがすべて確定したあとの1回の走査**
/// (この関数の後半)で行う。
#[test]
fn concurrent_lookups_observe_a_consistent_tree_while_inserts_are_in_flight() {
    const WRITERS: i64 = 4;
    const PER_WRITER: i64 = 300;
    const READERS: usize = 4;

    let path = temp_path("btree-concurrent-read-write");
    let btree = Arc::new(open_btree(&path));
    let barrier = Arc::new(Barrier::new(WRITERS as usize + READERS));
    let stop = Arc::new(AtomicBool::new(false));

    let writer_handles: Vec<_> = (0..WRITERS)
        .map(|t| {
            let btree = Arc::clone(&btree);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..PER_WRITER {
                    let key = t * PER_WRITER + i;
                    btree.insert(&Value::BigInt(key), dummy_rid()).unwrap();
                }
            })
        })
        .collect();
    let reader_handles: Vec<_> = (0..READERS)
        .map(|_| {
            let btree = Arc::clone(&btree);
            let barrier = Arc::clone(&barrier);
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                barrier.wait();
                while !stop.load(Ordering::Relaxed) {
                    for entry in btree.range(Bound::Unbounded, Bound::Unbounded).unwrap() {
                        entry.unwrap();
                    }
                }
            })
        })
        .collect();

    for h in writer_handles {
        h.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for h in reader_handles {
        h.join().unwrap();
    }

    // 書き込みがすべて確定したあとは、走査は決定的に昇順でなければならない。
    let mut previous: Option<i64> = None;
    let mut count = 0;
    for entry in btree.range(Bound::Unbounded, Bound::Unbounded).unwrap() {
        let (value, _rid) = entry.unwrap();
        let Value::BigInt(n) = value else { panic!("BigIntキーのはず") };
        if let Some(prev) = previous {
            assert!(prev < n, "書き込み確定後の走査が昇順を保っていない: {prev} -> {n}");
        }
        previous = Some(n);
        count += 1;
    }
    assert_eq!(count, (WRITERS * PER_WRITER) as usize);

    for t in 0..WRITERS {
        for i in 0..PER_WRITER {
            let key = t * PER_WRITER + i;
            assert_eq!(btree.lookup(&Value::BigInt(key)).unwrap().len(), 1);
        }
    }

    std::fs::remove_file(&path).unwrap();
}

/// `SharedDatabase`(第35章)経由で、複数スレッドが同じ行へ並行して
/// `UPDATE ... SET v = v + 1`を行っても更新を1件も失わない(Lost Updateの
/// 実スレッド版)。
///
/// 第31章のStrict 2PLは、ロックが取れない要求に`DbError::WouldBlock`を
/// **返すだけ**だった。決定的ハーネスはそれを見て「今は再試行しない」と
/// 判断する側に回っていたが、この章の`SharedDatabase::execute_in_tx`は
/// 同じ`WouldBlock`を受け取ったら`Condvar`でスレッドを待たせ、実際に
/// ブロックする。最終的な合計が`THREADS * ROUNDS`と正確に一致することが、
/// 更新を1件も失っていない証拠になる。
#[test]
fn concurrent_updates_to_the_same_row_via_shared_database_lose_no_update() {
    const THREADS: usize = 6;
    const ROUNDS: usize = 40;

    let db = common::temp_db();
    let shared = Arc::new(SharedDatabase::new(db));
    {
        let handle = shared.begin_tx();
        shared.execute_in_tx(&handle, "CREATE TABLE counters (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
        shared.execute_in_tx(&handle, "INSERT INTO counters VALUES (1, 0)").unwrap();
        shared.commit_tx(handle).unwrap();
    }

    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|_| {
            let shared = Arc::clone(&shared);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for _ in 0..ROUNDS {
                    let handle = shared.begin_tx();
                    shared.execute_in_tx(&handle, "UPDATE counters SET v = v + 1 WHERE id = 1").unwrap();
                    shared.commit_tx(handle).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let handle = shared.begin_tx();
    let result = shared.execute_in_tx(&handle, "SELECT v FROM counters WHERE id = 1").unwrap();
    shared.commit_tx(handle).unwrap();
    let Value::BigInt(total) = result.rows()[0].values()[0] else { panic!("BigIntのはず") };
    assert_eq!(total, (THREADS * ROUNDS) as i64, "同じ行への並行UPDATEがLost Updateを起こしている");
}

/// `SharedDatabase`経由で、複数スレッドが同じテーブルへ互いに素な行を
/// 並行して`INSERT`しても、最終的な行数が正確に一致する。
#[test]
fn concurrent_inserts_via_shared_database_produce_the_expected_row_count() {
    const THREADS: i64 = 6;
    const PER_THREAD: i64 = 50;

    let db = common::temp_db();
    let shared = Arc::new(SharedDatabase::new(db));
    {
        let handle = shared.begin_tx();
        shared.execute_in_tx(&handle, "CREATE TABLE items (id BIGINT PRIMARY KEY, owner BIGINT NOT NULL)").unwrap();
        shared.commit_tx(handle).unwrap();
    }

    let barrier = Arc::new(Barrier::new(THREADS as usize));
    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let shared = Arc::clone(&shared);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..PER_THREAD {
                    let id = t * PER_THREAD + i;
                    let handle = shared.begin_tx();
                    let sql = format!("INSERT INTO items VALUES ({id}, {t})");
                    shared.execute_in_tx(&handle, &sql).unwrap();
                    shared.commit_tx(handle).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let handle = shared.begin_tx();
    let result = shared.execute_in_tx(&handle, "SELECT id FROM items").unwrap();
    shared.commit_tx(handle).unwrap();
    assert_eq!(result.rows().len(), (THREADS * PER_THREAD) as usize);
}

/// 実スレッドでもデッドロック検出が機能する。
///
/// T1は口座1→2、T2は口座2→1という逆順のロック取得を行うよう仕組んである。
/// `Barrier`で「両者とも1件目の`UPDATE`(自分の送金元口座)を確定させた」
/// 直後に揃えてから2件目(相手の口座)へ進ませるため、互いに相手の持つ
/// ロックを待ち合う場面が必ず作られる。
///
/// Victim Selection(第32章)は循環の中で最も新しい`TransactionId`を選ぶため、
/// 先に`begin_tx`したT1が生き残り、後から`begin_tx`したT2が必ずVictimになる
/// (どちらのスレッドが先に2件目の要求を出すかという実行順には依存しない)。
/// この決定性を使って、実行順序を一切アサートせずに最終残高だけを検証する。
///
/// 実スレッドが本当にデッドロックしたまま(検出されずに)止まっていれば、
/// このテスト自体がハングする。ハングしたままCIを止めないよう、
/// 別スレッドからの完了通知に上限時間を設ける。
#[test]
fn deadlock_between_two_real_threads_is_detected_and_resolved() {
    // Memoryバックエンドはテーブル単位でしかロックを掛けない
    // (`crate::database`モジュール冒頭「ロックの粒度」を参照)ため、行1・行2
    // という異なる行を更新するだけの1件目からすでにテーブル全体で衝突して
    // しまい、この章が検証したい「2件目で初めて交差する」デッドロックを
    // 再現できない。Diskバックエンド(`Database::open`)は行(`RecordId`)単位の
    // ロックを持つため、こちらを使う。
    let path = common::temp_db_path("real-thread-deadlock");
    let db = minidb::Database::open(&path).unwrap();
    let shared = Arc::new(SharedDatabase::new(db));
    {
        let handle = shared.begin_tx();
        shared.execute_in_tx(&handle, "CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
        shared.execute_in_tx(&handle, "INSERT INTO accounts VALUES (1, 100)").unwrap();
        shared.execute_in_tx(&handle, "INSERT INTO accounts VALUES (2, 100)").unwrap();
        shared.commit_tx(handle).unwrap();
    }

    // Victim Selectionは最も新しい(最大の)TransactionIdを選ぶ。この決定性を
    // 使って実行順序を一切アサートせずに済ませるには、`begin_tx`自体を
    // スレッド生成より前、メインスレッドの中で順番に呼ぶ必要がある。
    // `begin_tx`をスレッドのクロージャの中で呼ぶと、どちらのスレッドが
    // 先にOSスケジューラに実行されるかによってTransactionIdの大小が
    // 決まってしまい、「先にbegin_txした側が生き残る」という前提が崩れる。
    let handle1 = shared.begin_tx();
    let handle2 = shared.begin_tx();

    let barrier = Arc::new(Barrier::new(2));
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    fn run_transfer(
        shared: &SharedDatabase,
        handle: minidb::TxHandle,
        barrier: &Barrier,
        from: i64,
        to: i64,
        done_tx: &std::sync::mpsc::Sender<()>,
    ) -> &'static str {
        shared.execute_in_tx(&handle, &format!("UPDATE accounts SET balance = balance - 1 WHERE id = {from}")).unwrap();
        barrier.wait();
        let result = shared.execute_in_tx(&handle, &format!("UPDATE accounts SET balance = balance + 1 WHERE id = {to}"));
        let outcome = match result {
            Ok(_) => {
                shared.commit_tx(handle).unwrap();
                "committed"
            }
            Err(DbError::DeadlockDetected) | Err(DbError::TransactionAborted) => {
                let _ = shared.rollback_tx(handle);
                "aborted"
            }
            Err(other) => panic!("想定外のエラー: {other}"),
        };
        let _ = done_tx.send(());
        outcome
    }

    let t1 = {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        let done_tx = done_tx.clone();
        thread::spawn(move || run_transfer(&shared, handle1, &barrier, 1, 2, &done_tx))
    };
    let t2 = {
        let shared = Arc::clone(&shared);
        let barrier = Arc::clone(&barrier);
        thread::spawn(move || run_transfer(&shared, handle2, &barrier, 2, 1, &done_tx))
    };

    // ハング検知: 10秒以内に両スレッドの完了通知が届かなければ、実スレッドの
    // デッドロックが検出されずに止まっていることを意味する。
    for _ in 0..2 {
        done_rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
            "実スレッドがデッドロックしたまま検出されていない(Condvarで待機したスレッドが\
             永久に起こされていない可能性がある)",
        );
    }

    let outcome1 = t1.join().unwrap();
    let outcome2 = t2.join().unwrap();

    // T1(先にbegin_txした側)が生き残り、T2(あとから始めた側)がVictimになる
    // (Victim Selectionは最も新しいTransactionIdを選ぶ、第32章)。
    assert_eq!(outcome1, "committed");
    assert_eq!(outcome2, "aborted");

    let handle = shared.begin_tx();
    let result = shared.execute_in_tx(&handle, "SELECT id, balance FROM accounts").unwrap();
    shared.commit_tx(handle).unwrap();
    let mut balances = std::collections::HashMap::new();
    for row in result.rows() {
        let Value::BigInt(id) = row.values()[0] else { panic!("BigInt") };
        let Value::BigInt(balance) = row.values()[1] else { panic!("BigInt") };
        balances.insert(id, balance);
    }
    // T1の送金だけが成立し、T2はAbortでUndoされているはず。
    assert_eq!(balances[&1], 99);
    assert_eq!(balances[&2], 101);

    drop(shared);
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(minidb_wal_path(&path)).ok();
}

/// FIFOの待ち行列の順序**だけ**が循環を閉じている3本のトランザクションの
/// 循環待ちを、実スレッドの上でも検出・解決できる。
///
/// `tests/deadlock.rs`の`a_cycle_closed_only_by_fifo_wait_queue_order_is_also_detected`
/// と同じ状況(T1がtaにSharedを持ち、T2がtaにExclusiveを要求してBlocked、
/// T3がtbのExclusiveを獲得したあとtaにSharedを要求してT2の後ろにFIFOで
/// 並ぶ、そこへT1がtbのExclusiveを要求してT1→T3→T2→T1の循環が閉じる)を
/// 実スレッドで再現する。
///
/// 待ち行列上の位置(どちらが先に`ta`を要求したか)が循環の成立に直結する
/// ため、ロックの状態そのものは決定的な(単一スレッドの)`Database`の上で
/// 先に組み立てる。`Database::execute_in_tx`は`WouldBlock`を**値**として
/// 返すだけで実スレッドを止めないので、この組み立て自体は`tests/deadlock.rs`
/// と同じ理由で完全に決定的である。組み立て終えたあとの`Database`を
/// `SharedDatabase`で包み、T2・T3の要求を実スレッドとして再発行させることで、
/// 「循環の解決によって手放されたロックを、`Condvar`で本当に眠っていた
/// スレッドが正しく引き継いで起き上がる」という、この章(第35章)が
/// 追加した層を検証する。
///
/// 実スレッドが本当にデッドロックしたまま止まっていれば、このテスト自体が
/// ハングする。ハングしたままCIを止めないよう、完了通知に上限時間を設ける。
#[test]
fn three_way_deadlock_closed_only_by_fifo_wait_queue_order_is_detected_on_real_threads() {
    let mut db = minidb::Database::memory();
    db.execute("CREATE TABLE ta (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("CREATE TABLE tb (id BIGINT PRIMARY KEY, v BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO ta VALUES (1, 0)").unwrap();
    db.execute("INSERT INTO tb VALUES (1, 0)").unwrap();

    // begin_txの順序どおりにTransactionIdが振られる。循環を最後に閉じる
    // T1自身が(TransactionIdが最大の)Victimになるよう、T1は最後にbegin_tx
    // する(`tests/deadlock.rs`の同名テストと同じ理由)。
    let t2 = db.begin_tx();
    let t3 = db.begin_tx();
    let t1 = db.begin_tx();

    // ここから3行、決定的な単一スレッドの`Database`の上でロックの待ち行列を
    // 組み立てる。T1がtaにSharedを持ち、T2のExclusive要求がBlockedになり、
    // T3がtbのExclusiveを獲得したあとtaのShared要求がT2の後ろにFIFOで並ぶ。
    db.execute_in_tx(&t1, "SELECT * FROM ta").unwrap();
    assert!(matches!(db.execute_in_tx(&t2, "UPDATE ta SET v = 2 WHERE id = 1"), Err(DbError::WouldBlock)));
    db.execute_in_tx(&t3, "UPDATE tb SET v = 3 WHERE id = 1").unwrap();
    assert!(matches!(db.execute_in_tx(&t3, "SELECT * FROM ta"), Err(DbError::WouldBlock)));

    // ここまでで組み立てたロックの状態を保ったまま、実スレッドから触れる
    // `SharedDatabase`へ包み直す。
    let shared = Arc::new(SharedDatabase::new(db));
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    // T2・T3は、さっきBlockedになったのと同じ文をもう一度発行する。待ち
    // 行列にはすでに自分自身が並んでいるので結果は変わらず、今度こそ
    // `SharedDatabase`が本物の`Condvar::wait`でスレッドを眠らせる。
    let t2_thread = {
        let shared = Arc::clone(&shared);
        let done_tx = done_tx.clone();
        thread::spawn(move || {
            let result = shared.execute_in_tx(&t2, "UPDATE ta SET v = 2 WHERE id = 1");
            if result.is_ok() {
                shared.commit_tx(t2).unwrap();
            }
            let _ = done_tx.send(());
            result
        })
    };
    let t3_thread = {
        let shared = Arc::clone(&shared);
        let done_tx = done_tx.clone();
        thread::spawn(move || {
            let result = shared.execute_in_tx(&t3, "SELECT * FROM ta");
            if result.is_ok() {
                shared.commit_tx(t3).unwrap();
            }
            let _ = done_tx.send(());
            result
        })
    };

    // T1がtbのExclusiveを要求すると、T3が保持するtbと衝突する(辺T1→T3)。
    // これにFIFOの辺(T3→T2)とモード衝突の辺(T2→T1)が合わさって循環が
    // 閉じ、循環内で最も新しいT1自身がVictimに選ばれる。
    let t1_result = shared.execute_in_tx(&t1, "UPDATE tb SET v = 10 WHERE id = 1");
    assert!(matches!(t1_result, Err(DbError::DeadlockDetected)), "T1が循環を閉じ、自らVictimになるはず");
    shared.rollback_tx(t1).unwrap();

    // T1がtaのSharedを手放したことで、待ち行列の先頭で眠っていたT2が
    // 起こされて昇格し、コミットして手放すと続けてT3も昇格する。
    for _ in 0..2 {
        done_rx.recv_timeout(std::time::Duration::from_secs(10)).expect(
            "実スレッドがデッドロックの解決後も起こされていない(Condvarで待機したスレッドが\
             永久に起こされていない可能性がある)",
        );
    }

    assert!(t2_thread.join().unwrap().is_ok(), "T2はT1のUndo後に昇格して完走するはず");
    assert!(t3_thread.join().unwrap().is_ok(), "T3はT2のCOMMIT後に昇格して完走するはず");

    let handle = shared.begin_tx();
    let result = shared.execute_in_tx(&handle, "SELECT v FROM ta WHERE id = 1").unwrap();
    let tb_result = shared.execute_in_tx(&handle, "SELECT v FROM tb WHERE id = 1").unwrap();
    shared.commit_tx(handle).unwrap();
    let Value::BigInt(ta_v) = result.rows()[0].values()[0] else { panic!("BigInt") };
    let Value::BigInt(tb_v) = tb_result.rows()[0].values()[0] else { panic!("BigInt") };
    assert_eq!(ta_v, 2, "T2のtaへの更新は生き残る");
    assert_eq!(tb_v, 3, "T3のtbへの更新は生き残る(VictimになったT1のtbへの書き込みはUndoされる)");
}

/// READ COMMITTEDのSELECTが実スレッドで一度`WouldBlock`により`Condvar::wait`
/// で眠り、先行トランザクションのCOMMITによって待ち行列から起こされて
/// Sharedロックを引き継いだ場合でも、そのSharedロックは文末で正しく解放
/// されなければならない(`tests/isolation_levels.rs`の
/// `read_committed_select_granted_after_waiting_is_still_released_at_statement_end`
/// と同じ状況を、`SharedDatabase`経由の実スレッドで確認する。第5部レビュー
/// 2巡目対応)。
///
/// ロックの初期状態(T1がExclusiveを保持、T2が`WouldBlock`で待ち行列に
/// 並んだところ)は、`tests/deadlock.rs`と同じ理由で決定的な(単一スレッドの)
/// `Database`の上で先に組み立てる。組み立てたあとの`Database`を
/// `SharedDatabase`で包み、T2の再試行を実スレッドとして再発行させることで、
/// 「待ち行列からの昇格によって、`Condvar`で本当に眠っていたスレッドが
/// 正しく起き上がり、かつその文の終わりでShared Lockを正しく手放す」ことを
/// 実スレッド上で確認する。
///
/// 実スレッドが本当にデッドロック・ハングしたまま止まっていれば、この
/// テスト自体がハングする。ハングしたままCIを止めないよう、完了通知に
/// 上限時間を設ける。
#[test]
fn read_committed_shared_lock_granted_after_a_real_thread_wait_is_still_released_at_statement_end() {
    let path = common::temp_db_path("real-thread-read-committed-granted-after-wait");
    let mut db = minidb::Database::open(&path).unwrap();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();

    let t1 = db.begin_tx();
    let t2 = db.begin_tx_with_isolation(minidb::IsolationLevel::ReadCommitted);
    let t3 = db.begin_tx_with_isolation(minidb::IsolationLevel::ReadCommitted);

    // T1がExclusiveを獲得する。
    db.execute_in_tx(&t1, "UPDATE accounts SET balance = 70 WHERE id = 1").unwrap();
    // T2のSELECTはT1のExclusiveとぶつかりWouldBlock(待ち行列に並ぶ)。
    assert!(matches!(
        db.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1"),
        Err(DbError::WouldBlock)
    ));

    let shared = Arc::new(SharedDatabase::new(db));
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    // T2は同じ文をもう一度発行する。待ち行列にはすでに自分自身が並んでいる
    // ので結果は変わらず、今度こそ`SharedDatabase`が本物の`Condvar::wait`で
    // スレッドを眠らせる。
    let t2_thread = {
        let shared = Arc::clone(&shared);
        thread::spawn(move || {
            let result = shared.execute_in_tx(&t2, "SELECT balance FROM accounts WHERE id = 1");
            let _ = done_tx.send(());
            result
        })
    };

    // T1がCOMMITすると、待ち行列に並んでいたT2のSharedが昇格し、notify_all
    // で眠っていたT2のスレッドが起こされる。
    shared.commit_tx(t1).unwrap();

    done_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("実スレッドが起こされていない(Condvarで待機したスレッドが永久に眠り続けている可能性がある)");
    let t2_result = t2_thread.join().unwrap();
    assert!(t2_result.is_ok(), "T2のSELECTはT1のCOMMIT後に成功するはず");
    let Value::BigInt(balance) = t2_result.unwrap().rows()[0].values()[0] else { panic!("BigInt") };
    assert_eq!(balance, 70);

    // T2のSELECTがすでに完了しているので、READ COMMITTEDの規律どおり
    // T2のSharedはこの時点で解放済みのはずである。T3のUPDATEはブロック
    // されずに進めなければならない。
    assert!(
        shared.execute_in_tx(&t3, "UPDATE accounts SET balance = 999 WHERE id = 1").is_ok(),
        "実スレッド上で待ち行列から付与されたSharedロックが、文末で解放されずに残っている"
    );
    shared.commit_tx(t2).unwrap();
    shared.commit_tx(t3).unwrap();

    let handle = shared.begin_tx();
    let result = shared.execute_in_tx(&handle, "SELECT balance FROM accounts WHERE id = 1").unwrap();
    shared.commit_tx(handle).unwrap();
    let Value::BigInt(balance) = result.rows()[0].values()[0] else { panic!("BigInt") };
    assert_eq!(balance, 999);

    drop(shared);
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(minidb_wal_path(&path)).ok();
}

/// `Database::open`が使うWALファイルのパス(`{path}.wal`)。テスト後の後片付け
/// にだけ使う。
fn minidb_wal_path(db_path: &std::path::Path) -> std::path::PathBuf {
    let mut wal = db_path.as_os_str().to_owned();
    wal.push(".wal");
    std::path::PathBuf::from(wal)
}

/// `--release`ビルドでも同じ結果になることを確認する意味合いのテスト。
/// 最適化でスレッドの実行速度・スケジューリングが変わっても、検証している
/// のは最終件数という決定的な性質だけなので結果は揺れない。
#[test]
fn concurrent_btree_inserts_are_deterministic_regardless_of_build_profile() {
    const THREADS: i64 = 4;
    const PER_THREAD: i64 = 150;

    let path = temp_path("btree-concurrent-insert-profile");
    let btree = Arc::new(open_btree(&path));
    let barrier = Arc::new(Barrier::new(THREADS as usize));

    let handles: Vec<_> = (0..THREADS)
        .map(|t| {
            let btree = Arc::clone(&btree);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..PER_THREAD {
                    let key = t * PER_THREAD + i;
                    btree.insert(&Value::BigInt(key), dummy_rid()).unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }

    let mut count = 0;
    for entry in btree.range(Bound::Unbounded, Bound::Unbounded).unwrap() {
        entry.unwrap();
        count += 1;
    }
    assert_eq!(count, (THREADS * PER_THREAD) as usize);

    std::fs::remove_file(&path).unwrap();
}
