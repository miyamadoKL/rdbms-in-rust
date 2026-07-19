//! クエリの協調的キャンセル・タイムアウト・メモリ上限をまとめた実行制御(第38章)。
//!
//! # なぜ「協調的」なのか
//!
//! OSのスレッドを外側から強制終了させる手段(`pthread_cancel`相当)は、
//! Rustの標準ライブラリには無い。`std::thread::JoinHandle`に「今すぐ止めろ」と
//! 伝えるAPIは存在せず、仮にあったとしても、ロックを保持したまま、あるいは
//! `Storage`の途中状態を書きかけのまま強制停止すれば、他のスレッドやファイルを
//! 壊れた状態のまま残しかねない。この章が採るのはその対極、**協調的
//! キャンセル**である。[`CancellationToken`]という「止めてほしい」という意思
//! 表示だけを渡し、実行中のコードの側が要所(本文で説明する「同期ポイント」)で
//! 自発的にこの意思表示を確認し、見つけたらその場でエラーを返して自分から
//! 終了する。強制力は無い代わりに、確認する場所を実行中のコード自身が選べる
//! ため、ロックや途中状態を安全に片付けてから抜けられる。
//!
//! # [`CancellationToken`]: 1本の`Arc<AtomicBool>`と締切
//!
//! [`CancellationToken`]は、`Arc<AtomicBool>`(明示的なキャンセル要求の有無)と
//! `Option<Instant>`(タイムアウトの締切)を1個ずつ持つ。[`CancellationToken::check`]は
//! この2つを順に見て、どちらかが成立していれば[`DbError::QueryCancelled`]・
//! [`DbError::QueryTimeout`]を返す。`Arc`なので`clone`は安価であり、`clone`した
//! 先はすべて同じキャンセル要求を共有する(タイムアウトの締切は`clone`後も
//! 同じ`Instant`のままなので、こちらも共有される)。
//!
//! `checkpoints`という`Arc<AtomicUsize>`も併せて持つ。`check`を呼ぶたびに
//! 1つずつ増える、単なる呼び出し回数のカウンタである。本番のコードはこの値を
//! 一切参照しない。テストが「別スレッドが実行中のクエリを、実際にキャンセルが
//! 効く前に完走させてしまわないタイミングで確実に`cancel`する」ために使う
//! (本文・テストのコメントを参照。`std::thread::sleep`による時間ベースの
//! 同期は、実行環境の速度に依存して稀に失敗する)。
//!
//! # どこで`check`するか
//!
//! `next()`を呼ぶたびにチェックすれば理論上はどこよりも早く気付けるが、
//! `AtomicBool::load`はコストがゼロではない。この章では、行を1件処理する
//! たびに定数コストの`check`が乗っても実害が無い場所(`Executor::next()`を
//! 駆動する最上位のループ、`Sort`・`Hash Join`・`Hash Aggregate`が子を`None`まで
//! 読み切る収集ループ)にだけ`check`を差し込む。ロック待ち(`DbError::WouldBlock`
//! の再試行、`crate::database::SharedDatabase`の`Condvar::wait`)はこの章では
//! `check`の対象にしていない(本文の限界节を参照)。
//!
//! # [`ExecutionContext`]: 文1本ぶんの実行制御をまとめる
//!
//! [`ExecutionContext`]は[`CancellationToken`]に加えて`max_operator_rows`
//! (Sort・Hash Join・Hash Aggregateの収集バッファに対する行数上限)を持つ。
//! `Session`が文を1本実行するたびに1個作り、`Database`の実行経路(`build_query_executor`・
//! `SortExec::new`等)へ`&ExecutionContext`として配る。バイト数ではなく行数で
//! 上限を決めているのは、`Tuple`が持つ`TEXT`列の実バイト数をたどるコストを
//! 払わずに済む単純さを優先したためであり、正確なメモリ使用量そのものを
//! 見積もる設計は演習課題に譲る(本文を参照)。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{DbError, DbResult};

/// キャンセル要求・タイムアウトの締切をまとめて持つ、`clone`可能な合図。
/// モジュール冒頭を参照。
#[derive(Clone)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
    checkpoints: Arc<AtomicUsize>,
}

impl CancellationToken {
    /// キャンセルされることも、締切を持つことも無い、恒久的に有効なトークン。
    /// `Database::execute`(この章より前から存在するAPI)のように、実行制御の
    /// 概念そのものを知らない呼び出し経路の既定値として使う(本文を参照)。
    pub fn inert() -> Self {
        CancellationToken { cancelled: Arc::new(AtomicBool::new(false)), deadline: None, checkpoints: Arc::new(AtomicUsize::new(0)) }
    }

    /// `flag`(外部から`store(true, ...)`されうる合図)を明示的なキャンセル要求に、
    /// `timeout`があればそれをタイムアウトの締切に使う新しいトークンを作る。
    ///
    /// `flag`を外から渡せるようにしているのは、`crate::session::Session`が
    /// 接続1本につき1個の`Arc<AtomicBool>`を持ち回し、文をまたいで同じ`Arc`を
    /// 使い回すためである([`crate::session::Session::cancellation_handle`]の
    /// ドキュメントを参照)。
    pub fn new(flag: Arc<AtomicBool>, timeout: Option<Duration>) -> Self {
        Self::with_checkpoints(flag, timeout, Arc::new(AtomicUsize::new(0)))
    }

    /// [`CancellationToken::new`]の、`checkpoints`(呼び出し回数のカウンタ)も
    /// 外から共有できる版。[`crate::session::Session::cancellation_handle`]が
    /// 返すトークンと、実際に実行中の文が使うトークンとで同じ`checkpoints`を
    /// 共有させることで、テストコードが「実行中の文がすでに同期ポイントを
    /// 何度も通過した」ことを、外から`checkpoints()`越しに観測できるようにする
    /// (モジュール冒頭を参照)。
    pub fn with_checkpoints(flag: Arc<AtomicBool>, timeout: Option<Duration>, checkpoints: Arc<AtomicUsize>) -> Self {
        CancellationToken { cancelled: flag, deadline: timeout.map(|d| Instant::now() + d), checkpoints }
    }

    /// この呼び出し回数を1つ進めたうえで、明示的なキャンセル要求・タイムアウトの
    /// 締切のどちらかが成立していれば`Err`を返す。同期ポイント(モジュール冒頭を
    /// 参照)から呼ぶ。
    pub fn check(&self) -> DbResult<()> {
        self.checkpoints.fetch_add(1, Ordering::Relaxed);
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            return Err(DbError::QueryTimeout);
        }
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(DbError::QueryCancelled);
        }
        Ok(())
    }

    /// 明示的なキャンセル要求を立てる。`crate::server`が接続の切断を検知した
    /// ときと、テストが「別スレッドから実行中のクエリをキャンセルする」ときに使う。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// これまでに[`CancellationToken::check`]が呼ばれた回数。本番のコードは
    /// 使わない、テスト専用の同期用カウンタ(モジュール冒頭を参照)。
    pub fn checkpoints(&self) -> usize {
        self.checkpoints.load(Ordering::Relaxed)
    }

    /// [`CancellationToken::checkpoints`]が`at_least`以上になるまでスピン
    /// ウェイトする。テストが「実行中のクエリが同期ポイントを確かに何度も
    /// 通過した」状態を、`sleep`による時間ベースの勘ではなく、実際の通過回数で
    /// 確認するために使う。
    pub fn wait_for_checkpoints(&self, at_least: usize) {
        while self.checkpoints() < at_least {
            std::hint::spin_loop();
        }
    }
}

/// 文1本ぶんの実行制御(キャンセル・タイムアウト・メモリ上限)をまとめたもの。
/// モジュール冒頭を参照。
#[derive(Clone)]
pub struct ExecutionContext {
    /// キャンセル・タイムアウトの合図。
    pub cancel: CancellationToken,
    /// `Sort`・`Hash Join`のBuild側・`Hash Aggregate`が子から集める行数の上限。
    /// `None`なら無制限(この章より前の挙動のまま)。
    pub max_operator_rows: Option<usize>,
}

impl ExecutionContext {
    /// キャンセルもタイムアウトもメモリ上限も課さない、この章より前の挙動と
    /// 同じ実行制御。[`Database::execute`](この章より前から存在するAPI)の
    /// 既定値として使う。
    pub fn unbounded() -> Self {
        ExecutionContext { cancel: CancellationToken::inert(), max_operator_rows: None }
    }

    /// `rows`(現在までにこの演算子が収集した行数)が`max_operator_rows`を
    /// 超えていたら[`DbError::MemoryLimitExceeded`]を返す。`operator`は
    /// エラーメッセージに載せる演算子名(`"Sort"`・`"Hash Join"`・
    /// `"Hash Aggregate"`)。
    pub fn check_row_limit(&self, operator: &'static str, rows: usize) -> DbResult<()> {
        if let Some(limit) = self.max_operator_rows
            && rows > limit
        {
            return Err(DbError::MemoryLimitExceeded { operator, limit });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn inert_token_never_reports_cancelled_or_timed_out() {
        let token = CancellationToken::inert();
        for _ in 0..1000 {
            assert!(token.check().is_ok());
        }
    }

    #[test]
    fn explicit_cancel_is_observed_by_every_clone() {
        let token = CancellationToken::new(Arc::new(AtomicBool::new(false)), None);
        let clone = token.clone();
        assert!(clone.check().is_ok());
        token.cancel();
        assert!(matches!(clone.check(), Err(DbError::QueryCancelled)));
    }

    #[test]
    fn timeout_fires_after_the_deadline_passes() {
        let token = CancellationToken::new(Arc::new(AtomicBool::new(false)), Some(Duration::from_millis(1)));
        thread::sleep(Duration::from_millis(50));
        assert!(matches!(token.check(), Err(DbError::QueryTimeout)));
    }

    #[test]
    fn checkpoints_count_every_check_call() {
        let token = CancellationToken::inert();
        for _ in 0..5 {
            token.check().unwrap();
        }
        assert_eq!(token.checkpoints(), 5);
    }

    #[test]
    fn wait_for_checkpoints_unblocks_once_another_thread_reaches_the_count() {
        let token = CancellationToken::inert();
        let worker = token.clone();
        let handle = thread::spawn(move || {
            for _ in 0..200 {
                worker.check().unwrap();
            }
        });
        token.wait_for_checkpoints(200);
        handle.join().unwrap();
        assert!(token.checkpoints() >= 200);
    }

    #[test]
    fn max_operator_rows_none_never_rejects() {
        let ctx = ExecutionContext::unbounded();
        assert!(ctx.check_row_limit("Sort", usize::MAX).is_ok());
    }

    #[test]
    fn max_operator_rows_some_rejects_once_exceeded() {
        let ctx = ExecutionContext { cancel: CancellationToken::inert(), max_operator_rows: Some(3) };
        assert!(ctx.check_row_limit("Sort", 3).is_ok());
        let err = ctx.check_row_limit("Sort", 4).unwrap_err();
        assert!(matches!(err, DbError::MemoryLimitExceeded { operator: "Sort", limit: 3 }));
    }
}
