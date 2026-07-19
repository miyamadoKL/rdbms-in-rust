//! Lock ManagerとStrict 2PL(第31章)。
//!
//! この章の`Database`は、複数のトランザクションが同じ行・同じテーブルへ
//! 同時にアクセスしようとしたとき、どちらか一方を**待たせる**という手段を
//! 初めて持つ。それまで(第30章まで)は、`INSERT`・`UPDATE`・`DELETE`・
//! `SELECT`はどのトランザクションの変更も即座に共有された`Backend`へ反映し、
//! 他のトランザクションはそれを`COMMIT`より前から見ることができた
//! (`tests/interleave.rs`が固定した、Lost Update・Dirty Read・
//! Non-repeatable Read・Phantomという4つの異常)。
//!
//! この章のLock Managerは、行(またはテーブル)ごとに「今どのトランザクションが
//! どんなロックを持っているか」を記録し、両立しないロックの要求を
//! [`LockResult::Blocked`]として突き返す。この`Blocked`が、上の4つの異常のうち
//! 3つ(Lost Update・Dirty Read・Non-repeatable Read)を実際に防ぐ土台になる
//! (Phantomがなぜ防げないままなのかは、この章の本文と`src/database.rs`の
//! ロック粒度の説明を参照)。
//!
//! # 実スレッドを待たせない
//!
//! 本物のLock Managerは、ロックを取れないトランザクションのスレッドを
//! 実際にブロックし(`Mutex`の`lock()`が返ってこないのと同じ意味で)、ロックが
//! 解放されたときにOSのスケジューラがそのスレッドを起こす。このクレートは
//! 第35章までBuffer PoolとB+Treeがスレッドセーフでなく、複数スレッドから
//! 同じ`Database`を触れない(第30章の決定的インターリーブテストハーネスの
//! ドキュメントを参照)。そのため、この章の[`LockManager::acquire`]は
//! ブロックする代わりに[`LockResult::Blocked`]という**値**を返すだけで、
//! 即座に呼び出し元へ制御を返す。呼び出し元(`Database::execute_in_tx`)は、
//! ロックを取れなかった文を実行せずに`Err`として返し、そのトランザクションを
//! `Active`のまま保つ。ロックを取れなかった文をもう一度試すかどうか、いつ
//! 試すかは、呼び出し側(ハーネスを使うテストコード)が決める。この章は
//! 「取れなければ待ち行列に並べておく」ところまでで、待っているトランザクションを
//! 自動的に起こして再実行するスケジューラは持たない。

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;

use crate::ids::{RecordId, TableId, TransactionId};

/// SharedロックとExclusiveロックの2種類。
///
/// # 互換性行列
///
/// | 保持中\要求 | Shared | Exclusive |
/// |---|---|---|
/// | Shared | 両立する | 両立しない |
/// | Exclusive | 両立しない | 両立しない |
///
/// Shared同士だけが両立する。1つのキーに対して複数のトランザクションが
/// 同時に`Shared`を持てるが、`Exclusive`はどんな組み合わせであっても
/// 他のロックと同居できない(自分自身がすでに持っている場合を除く。
/// [`LockManager::acquire`]を参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    /// 読み取り用。複数のトランザクションが同じ対象に同時に持てる。
    Shared,
    /// 書き込み用。1つのトランザクションしか同時に持てない。
    Exclusive,
}

impl LockMode {
    /// `self`(保持中のモード)と`other`(要求されたモード)が両立するか。
    fn compatible_with(self, other: LockMode) -> bool {
        matches!((self, other), (LockMode::Shared, LockMode::Shared))
    }
}

/// [`LockManager::acquire`]の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockResult {
    /// ロックを獲得できた。呼び出し元はそのまま処理を続けてよい。
    Granted,
    /// 他のトランザクションが両立しないロックを持っているため、獲得できな
    /// かった。要求は待ち行列の末尾(Lock Upgradeの場合は先頭、本文を参照)に
    /// 積まれており、対象のロックが解放されるたびに[`LockManager::release_all`]
    /// が再評価する。呼び出し元は今すぐこの要求を諦めるのではなく、あとで
    /// もう一度同じ要求を試すことを想定している(モジュール冒頭の説明を参照)。
    Blocked,
}

/// [`crate::database::Database`]がSELECT・DMLの対象を指すために使うロックの
/// 単位。
///
/// `Table`はテーブル全体を1個のロック対象として扱う(この章の前半で作る、
/// 最初の粒度)。`Tuple`はテーブルの1行(`RecordId`)を1個のロック対象として
/// 扱う、より細かい粒度である。どちらの粒度を実際に使うかは`Database`の
/// バックエンド(Memory・Disk)によって決まる。理由は
/// [`crate::database`]モジュールの「ロックの粒度」節を参照。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LockKey {
    /// テーブル全体。
    Table(TableId),
    /// 1行(`RecordId`はDiskバックエンドだけが持つ、第30章の`transaction`
    /// モジュールを参照)。
    Tuple(TableId, RecordId),
}

/// 待ち行列に積まれた1件の要求。
struct Waiter {
    txn: TransactionId,
    mode: LockMode,
    /// この要求が、すでに`Shared`を持っているトランザクションによる
    /// `Exclusive`へのUpgrade要求かどうか。Upgrade要求は待ち行列の並び方が
    /// 通常の新規要求と違う(`LockManager::acquire`のドキュメントを参照)。
    is_upgrade: bool,
}

/// 1つのロック対象(`LockKey`1個)が持つ、保持者と待ち行列。
#[derive(Default)]
struct LockEntry {
    holders: Vec<(TransactionId, LockMode)>,
    waiters: VecDeque<Waiter>,
}

impl LockEntry {
    /// 現在の保持者全員と`mode`が両立するか。保持者がいなければ常に`true`。
    fn compatible_with_holders(&self, mode: LockMode) -> bool {
        self.holders.iter().all(|(_, held)| held.compatible_with(mode))
    }

    fn holder_mode(&self, txn: TransactionId) -> Option<LockMode> {
        self.holders.iter().find(|(t, _)| *t == txn).map(|(_, mode)| *mode)
    }
}

/// キー`K`ごとに、Shared/ExclusiveロックとWait Queueを管理する。
///
/// `K`はロックの対象を指す型で、この章では[`LockKey`]を渡す
/// (`Database`は`LockManager<LockKey>`を1個持つ)。`K`をジェネリクスに
/// したのは、この章がまず`TableId`だけを`K`に据えてTable Lockの正しさを
/// 単体で確認し(本文・`tests`内のユニットテストを参照)、そのあと`Database`へ
/// 組み込む段になって`LockKey`(Table・Tupleの両方を表せる型)へ差し替える、
/// という2段階の進め方を取るためである。ロックの管理ロジック自体はどちらの
/// 段でも1文字も変わらない。
pub struct LockManager<K: Eq + Hash + Clone> {
    entries: HashMap<K, LockEntry>,
}

impl<K: Eq + Hash + Clone> Default for LockManager<K> {
    fn default() -> Self {
        LockManager { entries: HashMap::new() }
    }
}

impl<K: Eq + Hash + Clone> LockManager<K> {
    /// ロックを1つも持たない、空のLock Managerを作る。
    pub fn new() -> Self {
        Self::default()
    }

    /// `txn`が`key`に対して`mode`のロックを要求する。
    ///
    /// # 3つの分岐
    ///
    /// 1. **すでに十分なロックを持っている**: `txn`が`key`に対してすでに
    ///    `mode`以上の強さのロックを持っていれば(`Exclusive`は`Shared`の
    ///    要求も含めて何でも満たす)、そのまま`Granted`を返す。
    /// 2. **Lock Upgrade**: `txn`がすでに`Shared`を持っていて、要求が
    ///    `Exclusive`のとき。`txn`がそのキーの唯一の保持者であれば、他の
    ///    誰とも衝突しないのでその場で`Shared`を`Exclusive`へ書き換えて
    ///    `Granted`を返す。他にも`Shared`を持つトランザクションがいれば、
    ///    Upgrade要求を待ち行列の**先頭**に積んで`Blocked`を返す。先頭に積む
    ///    理由は、`txn`は「新規に割り込んできた要求」ではなく「すでに部分的な
    ///    権利(Shared)を持っている既存の参加者」だからである。末尾に積むと、
    ///    あとから来た無関係な新規Shared要求に何度も追い越され、Upgradeだけが
    ///    いつまでも成立しない(Upgrade starvation)。
    /// 3. **新規要求**: `txn`がまだこのキーに触れていない場合。待ち行列が
    ///    空で、かつ現在の保持者全員と`mode`が両立すれば即座に`Granted`。
    ///    そうでなければ待ち行列の**末尾**に積んで`Blocked`を返す
    ///    (`Blocked`になった理由・待ち行列の並び順の意味は
    ///    [`LockManager::release_all`]のFIFOに関する説明を参照)。
    pub fn acquire(&mut self, txn: TransactionId, key: K, mode: LockMode) -> LockResult {
        let entry = self.entries.entry(key).or_default();

        if let Some(held) = entry.holder_mode(txn) {
            if held == LockMode::Exclusive || held == mode {
                return LockResult::Granted;
            }
            // held == Shared, mode == Exclusive: Lock Upgrade。
            if entry.holders.len() == 1 {
                entry.holders[0].1 = LockMode::Exclusive;
                return LockResult::Granted;
            }
            if !entry.waiters.iter().any(|w| w.txn == txn) {
                entry.waiters.push_front(Waiter { txn, mode, is_upgrade: true });
            }
            return LockResult::Blocked;
        }

        if entry.waiters.is_empty() && entry.compatible_with_holders(mode) {
            entry.holders.push((txn, mode));
            return LockResult::Granted;
        }
        if !entry.waiters.iter().any(|w| w.txn == txn) {
            entry.waiters.push_back(Waiter { txn, mode, is_upgrade: false });
        }
        LockResult::Blocked
    }

    /// `txn`が保持している(待ち行列に積んだままの要求も含む)すべてのロックを
    /// 手放す。Strict 2PLの一括解放(`COMMIT`・`ROLLBACK`)、およびAutocommit
    /// 文が1文の終わりに呼ぶ(`crate::database`の該当節を参照)。
    ///
    /// 解放したキーごとに、待ち行列の先頭から
    /// 昇格できるだけ昇格させる([`LockManager::promote_waiters`])。
    pub fn release_all(&mut self, txn: TransactionId) {
        let keys: Vec<K> = self
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.holders.iter().any(|(t, _)| *t == txn) || entry.waiters.iter().any(|w| w.txn == txn)
            })
            .map(|(key, _)| key.clone())
            .collect();

        for key in keys {
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.holders.retain(|(t, _)| *t != txn);
                entry.waiters.retain(|w| w.txn != txn);
                self.promote_waiters(&key);
            }
        }
    }

    /// `key`の待ち行列を先頭から見て、今の保持者集合と両立する限り昇格させる。
    ///
    /// # FIFOと後続のS要求がX待ちを追い越さない理由
    ///
    /// 待ち行列の先頭が`Shared`要求で、それが現在の保持者と両立すれば昇格し、
    /// 次の要求へ進む。これを繰り返すと、先頭に連続して並んだ`Shared`要求は
    /// まとめて昇格できる(`Shared`同士は何人いても両立するため)。ところが
    /// 先頭が`Exclusive`要求(または成立しないUpgrade要求)だった場合はそこで
    /// 止まる。たとえ2番目以降に、今なら通る`Shared`要求が控えていても、
    /// **先へは進まない**。先頭の`Exclusive`要求を飛び越して後続の`Shared`
    /// 要求を先に通してしまうと、`Exclusive`要求は後から来る`Shared`要求に
    /// 際限なく追い越され続け、いつまでもロックを取れなくなる
    /// (Starvation)。この関数が「先頭が両立しなければ即座に止める」という
    /// 単純な規則を守っているのは、この追い越しを起こさないためである。
    fn promote_waiters(&mut self, key: &K) {
        let Some(entry) = self.entries.get_mut(key) else { return };
        while let Some((is_upgrade, txn, mode)) = entry.waiters.front().map(|w| (w.is_upgrade, w.txn, w.mode)) {
            if is_upgrade {
                let is_sole_holder = entry.holders.len() == 1 && entry.holders[0].0 == txn;
                if !is_sole_holder {
                    break;
                }
                entry.waiters.pop_front();
                entry.holders[0].1 = LockMode::Exclusive;
                continue;
            }
            if !entry.compatible_with_holders(mode) {
                break;
            }
            entry.waiters.pop_front();
            entry.holders.push((txn, mode));
        }
        if entry.holders.is_empty() && entry.waiters.is_empty() {
            self.entries.remove(key);
        }
    }

    /// `key`に対して`txn`が現在保持しているロックの強さ。テストと
    /// デバッグ用の観測用途に限る(`Database`の通常の実行経路はこれを使わず、
    /// `acquire`の戻り値だけで判断する)。
    #[cfg(test)]
    fn holder_mode(&self, txn: TransactionId, key: &K) -> Option<LockMode> {
        self.entries.get(key).and_then(|entry| entry.holder_mode(txn))
    }

    /// `key`の待ち行列に並んでいる`TransactionId`を、先頭から順に返す。
    /// テスト専用(FIFO順序の検証に使う)。
    #[cfg(test)]
    fn waiting_order(&self, key: &K) -> Vec<TransactionId> {
        self.entries.get(key).map(|entry| entry.waiters.iter().map(|w| w.txn).collect()).unwrap_or_default()
    }

    /// 現在、待ち行列に1件以上並んでいる(=`Blocked`のまま止まっている)
    /// トランザクションの集合。デッドロックの**観測**用(この章は検出・解決を
    /// 行わない、本文「デッドロックはこの章では検出しない」を参照)。
    #[cfg(test)]
    fn blocked_transactions(&self) -> std::collections::HashSet<TransactionId> {
        self.entries.values().flat_map(|entry| entry.waiters.iter().map(|w| w.txn)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T1: TransactionId = TransactionId(1);
    const T2: TransactionId = TransactionId(2);
    const T3: TransactionId = TransactionId(3);

    fn table(n: u64) -> LockKey {
        LockKey::Table(TableId(n))
    }

    // ---- 互換性行列 ----

    #[test]
    fn shared_locks_are_compatible_with_each_other() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Shared), LockResult::Granted);
    }

    #[test]
    fn exclusive_lock_conflicts_with_an_existing_shared_lock() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Exclusive), LockResult::Blocked);
    }

    #[test]
    fn shared_lock_conflicts_with_an_existing_exclusive_lock() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Shared), LockResult::Blocked);
    }

    #[test]
    fn exclusive_lock_conflicts_with_an_existing_exclusive_lock() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Exclusive), LockResult::Blocked);
    }

    #[test]
    fn reacquiring_the_same_or_weaker_mode_is_a_no_op() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
        // すでにExclusiveを持っているので、Sharedの要求はそのまま素通りする。
        assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
        assert_eq!(lm.holder_mode(T1, &table(1)), Some(LockMode::Exclusive));
    }

    // ---- Lock Upgrade ----

    #[test]
    fn sole_shared_holder_upgrades_immediately() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
        assert_eq!(lm.holder_mode(T1, &table(1)), Some(LockMode::Exclusive));
    }

    #[test]
    fn upgrade_blocks_while_another_transaction_also_holds_shared() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Shared), LockResult::Granted);
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Blocked);

        // T2がSharedを手放すと、T1は唯一の保持者になりUpgradeが成立する。
        lm.release_all(T2);
        assert_eq!(lm.holder_mode(T1, &table(1)), Some(LockMode::Exclusive));
    }

    #[test]
    fn upgrade_request_is_inserted_ahead_of_new_shared_requests() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Shared), LockResult::Granted);
        // T1がUpgradeをブロックされたまま待つ。
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Blocked);
        // 後から来たT3の新規Shared要求は、末尾に積まれる。
        assert_eq!(lm.acquire(T3, table(1), LockMode::Shared), LockResult::Blocked);
        assert_eq!(lm.waiting_order(&table(1)), vec![T1, T3], "UpgradeのT1が新規要求のT3より先頭にいる");
    }

    // ---- Wait Queue: FIFOとStarvation回避 ----

    #[test]
    fn a_later_shared_request_does_not_overtake_an_earlier_exclusive_request() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Shared), LockResult::Granted);
        // T2のExclusiveはT1のSharedとぶつかりBlocked。
        assert_eq!(lm.acquire(T2, table(1), LockMode::Exclusive), LockResult::Blocked);
        // T3の新規SharedはT1とは両立するはずだが、待ち行列の先頭(T2)を
        // 追い越せないため、やはりBlockedになる。
        assert_eq!(lm.acquire(T3, table(1), LockMode::Shared), LockResult::Blocked);
        assert_eq!(lm.waiting_order(&table(1)), vec![T2, T3]);

        // T1が手放すと、先頭のT2(Exclusive)だけが昇格し、T3はまだ待つ。
        lm.release_all(T1);
        assert_eq!(lm.holder_mode(T2, &table(1)), Some(LockMode::Exclusive));
        assert_eq!(lm.waiting_order(&table(1)), vec![T3]);

        // T2が手放して初めて、T3が通る。
        lm.release_all(T2);
        assert_eq!(lm.holder_mode(T3, &table(1)), Some(LockMode::Shared));
    }

    #[test]
    fn consecutive_shared_waiters_are_promoted_together() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Shared), LockResult::Blocked);
        assert_eq!(lm.acquire(T3, table(1), LockMode::Shared), LockResult::Blocked);

        lm.release_all(T1);
        assert_eq!(lm.holder_mode(T2, &table(1)), Some(LockMode::Shared));
        assert_eq!(lm.holder_mode(T3, &table(1)), Some(LockMode::Shared));
        assert!(lm.waiting_order(&table(1)).is_empty());
    }

    // ---- 一括解放 ----

    #[test]
    fn release_all_frees_every_key_a_transaction_holds() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
        assert_eq!(lm.acquire(T1, table(2), LockMode::Shared), LockResult::Granted);

        lm.release_all(T1);

        assert_eq!(lm.holder_mode(T1, &table(1)), None);
        assert_eq!(lm.holder_mode(T1, &table(2)), None);
        // 誰も持っていないキーは新規要求がそのまま通る。
        assert_eq!(lm.acquire(T2, table(1), LockMode::Exclusive), LockResult::Granted);
    }

    #[test]
    fn release_all_also_removes_pending_wait_entries() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Shared), LockResult::Blocked);

        // T2は自分自身の要求(まだ成立していないもの)を取り下げる。
        lm.release_all(T2);
        assert!(lm.waiting_order(&table(1)).is_empty());

        lm.release_all(T1);
        assert_eq!(lm.holder_mode(T2, &table(1)), None, "取り下げた要求は昇格しない");
    }

    // ---- デッドロックの観測(検出はしない) ----

    #[test]
    fn mutual_wait_leaves_both_transactions_blocked_without_detection() {
        let mut lm = LockManager::new();
        assert_eq!(lm.acquire(T1, table(1), LockMode::Exclusive), LockResult::Granted);
        assert_eq!(lm.acquire(T2, table(2), LockMode::Exclusive), LockResult::Granted);

        // T1はT2の持つtable(2)を、T2はT1の持つtable(1)を欲しがる。
        assert_eq!(lm.acquire(T1, table(2), LockMode::Exclusive), LockResult::Blocked);
        assert_eq!(lm.acquire(T2, table(1), LockMode::Exclusive), LockResult::Blocked);

        // どちらも自動的には解決されない。このLock Manager自身はデッドロック
        // 検出を行わないため、2つとも待ち行列に残ったままである
        // (検出・解決は第32章のWait-for Graph)。
        let blocked = lm.blocked_transactions();
        assert!(blocked.contains(&T1));
        assert!(blocked.contains(&T2));
    }
}
