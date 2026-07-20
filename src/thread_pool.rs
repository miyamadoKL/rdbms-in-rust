//! 接続を処理する固定サイズのワーカースレッドプール(第38章)。
//!
//! # 前章の問題: 接続ごとに無制限にスレッドが増える
//!
//! 第36章の`crate::server`は、接続を受け付けるたびに`std::thread::spawn`で
//! スレッドを1本立てていた。1,000個の接続を同時に張られれば1,000本の
//! OSスレッドが生まれる。1本ごとのスタック(既定8MiB程度)とOSのスケジューリング
//! コストを考えれば、この方式は接続数の上限を「クライアントがどれだけ同時に
//! つながるか」という、サーバー側が制御できない値にゆだねてしまっている。
//!
//! この章は、接続を処理するスレッドの本数を[`WorkerPool::new`]の
//! `worker_count`で固定する。同時に処理できる接続数の上限は、常にこの
//! 定数で頭打ちになる。
//!
//! # 上限を超えたらどうするか: 待たせず拒否する
//!
//! `worker_count`本のワーカーが全員別の接続を処理中のとき、新しい接続を
//! どう扱うかには2つの選択肢がある。**キューに積んで待たせる**か、**即座に
//! 拒否する**かである。
//!
//! この章は拒否を選ぶ。[`WorkerPool::dispatch`]は、`queue_capacity`個までは
//! キューに積んで待たせるが、それも埋まっていれば`Err`で呼び出したTCP接続を
//! そのまま呼び出し元へ返す。`crate::server`はこの`Err`を受け取ったら、
//! その接続へエラー応答を1つ書いてから切断する(接続そのものは拒否しても、
//! 「拒否された」という事実はクライアントへ伝える)。
//!
//! 対する「待たせる」設計(`queue_capacity`を実質無制限にする)は、接続数の
//! 上限という当初の目的を弱める。同時に1,000個の接続が殺到すれば、たとえ
//! ワーカーが4本に固定されていても、999個の`TcpStream`がキューの中で
//! 待ち続けることになり、結局「同時に保持するリソースの数」は無制限のまま
//! 残ってしまう。即座に拒否すれば、クライアントは(待ち続けて何が起きているか
//! 分からないまま固まるより)すぐに「今は繋がらない」と知り、必要なら
//! 自分の判断で再接続を試みられる。
//!
//! `worker_count + queue_capacity`が、この章のサーバーが同時に保持する
//! 接続数の実質的な上限になる。
//!
//! # 実装: 固定長キュー1本を複数ワーカーで奪い合う
//!
//! `std::sync::mpsc::sync_channel(queue_capacity)`が、この章のキューそのもの
//! である。`sync_channel`は容量固定のバウンデッドチャネルで、容量を超えて
//! `try_send`すると`Err(TrySendError::Full)`を返す(このチャネルは
//! `std::sync::mpsc::Receiver`を複数スレッドで共有できない`!Sync`な型のため、
//! 受信側を`Mutex`で包み、`worker_count`本のワーカースレッドがこの
//! `Mutex`を奪い合って次の接続を受け取る)。

use std::net::TcpStream;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// 接続を処理するワーカースレッドの集合。モジュール冒頭を参照。
pub struct WorkerPool {
    /// `None`になるのは[`WorkerPool::join`]がSenderを`drop`した後だけ
    /// (モジュール冒頭「新規ジョブの受付を止める」を参照)。
    sender: Option<SyncSender<TcpStream>>,
    workers: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    /// `worker_count`本のワーカースレッドを立てる。各ワーカーは、キューから
    /// `TcpStream`を1本受け取るたびに`handler`を呼び、`handler`が戻ったら
    /// 次の`TcpStream`を待つ、というループを繰り返す。
    ///
    /// `handler`は`Fn`(`FnOnce`ではない)である。1本のワーカースレッドが
    /// 生きている間に何本もの接続を順に処理するため、`handler`はワーカーの
    /// 数だけ`clone`されるのではなく、`Arc`で全ワーカーに共有される。
    pub fn new<F>(worker_count: usize, queue_capacity: usize, handler: F) -> Self
    where
        F: Fn(TcpStream) + Send + Sync + 'static,
    {
        assert!(worker_count > 0, "ワーカースレッドは最低1本必要です");
        let (sender, receiver) = sync_channel::<TcpStream>(queue_capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let handler = Arc::new(handler);

        let workers = (0..worker_count)
            .map(|_| {
                let receiver = Arc::clone(&receiver);
                let handler = Arc::clone(&handler);
                thread::spawn(move || worker_loop(&receiver, handler.as_ref()))
            })
            .collect();

        WorkerPool { sender: Some(sender), workers }
    }

    /// `stream`をキューへ積む。空きが無ければ`stream`をそのまま`Err`で返す
    /// (モジュール冒頭「上限を超えたらどうするか」を参照)。
    pub fn dispatch(&self, stream: TcpStream) -> Result<(), TcpStream> {
        let sender = self.sender.as_ref().expect("shutdown後にdispatchは呼ばれない(Server::runの契約)");
        match sender.try_send(stream) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(stream)) => Err(stream),
            Err(TrySendError::Disconnected(_)) => {
                unreachable!("selfがsenderを保持している間、receiver側のworkerは必ず生きている")
            }
        }
    }

    /// 新規の`dispatch`を受け付けなくなるようSenderを`drop`し、キュー済み・
    /// 処理中の接続をすべてのワーカーが処理し終えるまで待つ(Graceful
    /// Shutdown、`crate::server`の本文を参照)。
    ///
    /// Senderを`drop`すると、キューが空になった時点で各ワーカーの`recv`が
    /// `Err`(Sender側が全て無くなった)を返すようになり、`worker_loop`の
    /// ループを抜けてスレッドが終了する。`JoinHandle::join`はそのスレッドの
    /// 終了を待つので、この関数が戻った時点で「新しい接続の受付は止まっており、
    /// それまでに受け付けていた接続はすべて処理を終えている」ことが保証される。
    pub fn join(mut self) {
        self.sender.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

fn worker_loop(receiver: &Mutex<Receiver<TcpStream>>, handler: &(dyn Fn(TcpStream) + Send + Sync)) {
    loop {
        let stream = {
            let guard = receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.recv()
        };
        match stream {
            Ok(stream) => handler(stream),
            Err(_) => break, // 全Senderがdropされた(WorkerPool::join)。
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// テスト用に、接続を1本受け付けるたびに`connect`できる`TcpStream`を作る。
    fn dummy_stream() -> TcpStream {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server_side, _) = listener.accept().unwrap();
        drop(server_side); // ハンドラへ渡すのはクライアント側のstreamで十分
        client
    }

    /// `queue_capacity`が`0`の`WorkerPool`へ`dispatch`する場合、`try_send`が
    /// 成功するには「ワーカーがすでに`recv`で待っている」ことが必要になる
    /// (モジュール冒頭のレンデブー方式を参照)。`WorkerPool::new`が返った直後は
    /// スレッドの起動がまだ完了していないことがあるため、ここでは失敗しても
    /// (`sleep`による時間待ちではなく)ワーカーが実際に受け取るまでリトライする。
    fn dispatch_retrying_until_accepted(pool: &WorkerPool, mut stream: TcpStream) {
        loop {
            match pool.dispatch(stream) {
                Ok(()) => return,
                Err(returned) => stream = returned,
            }
        }
    }

    #[test]
    fn dispatch_runs_the_handler_on_a_worker_thread() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_handler = Arc::clone(&counter);
        let pool = WorkerPool::new(2, 0, move |_stream| {
            counter_for_handler.fetch_add(1, Ordering::SeqCst);
        });

        dispatch_retrying_until_accepted(&pool, dummy_stream());
        dispatch_retrying_until_accepted(&pool, dummy_stream());
        pool.join();

        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    /// N+1本目の接続の挙動: ワーカー1本・キュー容量0のプールを、1本の
    /// ハンドラが解放されるまで塞いでおくと、その間に来た接続は`dispatch`が
    /// `Err`で拒否する。
    #[test]
    fn dispatch_rejects_once_the_single_worker_and_empty_queue_are_full() {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let started = Arc::new(AtomicUsize::new(0));
        let started_for_handler = Arc::clone(&started);

        let pool = WorkerPool::new(1, 0, move |_stream| {
            started_for_handler.fetch_add(1, Ordering::SeqCst);
            // ワーカーをここで足止めし、次の`dispatch`が確実に「空きが
            // 無い」状態を踏むようにする。
            let _ = release_rx.lock().unwrap().recv();
        });

        dispatch_retrying_until_accepted(&pool, dummy_stream());
        // ワーカーがハンドラに入るまで待つ(sleepではなく実際の進捗を確認する)。
        while started.load(Ordering::SeqCst) == 0 {
            std::hint::spin_loop();
        }

        let rejected = pool.dispatch(dummy_stream());
        assert!(rejected.is_err(), "唯一のワーカーが塞がっている間の2本目は拒否される");

        release_tx.send(()).unwrap();
        pool.join();
    }

    #[test]
    fn join_waits_for_in_flight_handlers_before_returning() {
        let finished = Arc::new(AtomicUsize::new(0));
        let finished_for_handler = Arc::clone(&finished);
        let pool = WorkerPool::new(1, 4, move |_stream| {
            thread::sleep(Duration::from_millis(20));
            finished_for_handler.fetch_add(1, Ordering::SeqCst);
        });

        for _ in 0..3 {
            pool.dispatch(dummy_stream()).unwrap();
        }
        pool.join();

        assert_eq!(finished.load(Ordering::SeqCst), 3, "joinはキュー済みの分もすべて処理してから戻る");
    }
}
