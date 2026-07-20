//! Blocking I/OのTCPサーバー(第36章)。ワーカースレッドプールとGraceful
//! Shutdown(第38章)。
//!
//! `crate::protocol`が定義するフレームを使って、複数のTCP接続を受け付け、
//! `crate::database::SharedDatabase`(第35章)を経由してSQLを実行する。
//!
//! # 第38章より前: 接続ごとに1本のOSスレッド
//!
//! 第36章の時点でのこのモジュールは、接続を受け付けるたびに
//! `std::thread::spawn`でスレッドを1本立てる、最も単純な並行モデルを採っていた。
//! 接続数が増えるとスレッド数がそのまま増え続けるため、大量の接続を長時間
//! 張られる用途には向かない。この章はこれを[`crate::thread_pool::WorkerPool`]
//! (固定サイズのワーカースレッドプール)へ置き換える。同時に処理できる接続数の
//! 上限、上限を超えた接続をどう扱うかは`crate::thread_pool`モジュール冒頭を
//! 参照。
//!
//! `SharedDatabase`はすでに`Mutex`1本でスレッド間排他を行っている(第35章)ため、
//! このモジュール自身がロックを追加で管理する必要は無い。複数の接続スレッドが
//! 同時にSQLを送ってきても、`SharedDatabase`の内側で直列化される。
//!
//! # 接続単位の状態: [`crate::session::Session`]
//!
//! 接続ごとのトランザクション状態・Prepared Statementの名前空間は
//! [`crate::session::Session`](第37章)が一元管理する。このモジュールの
//! 役目は、TCP接続を受け付けて`Session`を1個作り、フレームを読んでは
//! `Session::execute`へ渡し、返ってきた`QueryResult`をフレームへ書き戻す
//! ことだけである。
//!
//! # 切断時のトランザクション後始末
//!
//! 接続が(正常な`\q`ではなく)途中で切れた場合、そのセッションが持っていた
//! 未コミットのトランザクションをどうするかを決めておかないと、そのトランザクション
//! が獲得したロック(第31章)を他の接続が永久に待たされる。[`crate::session::Session`]の
//! `Drop`実装が、保持中の`TxHandle`があれば無条件に`rollback_tx`する。
//! 読み取り専用の接続がソケットエラーで終了した場合も`rollback_tx`を呼ぶが、
//! 対象のトランザクションはロックを1つも保持していないことがあり、その場合は
//! 何もしないのと実質的に同じである。`Drop`は正常な切断・異常な切断・
//! パニックのどの経路でも通るため、後始末を呼び出し側の分岐(切断理由ごとの
//! `match`)に分散させずに1箇所へ集約できる。
//!
//! # クライアント切断の検知と実行中クエリのキャンセル
//!
//! 文を1本実行している間、このモジュールは[`watch_for_disconnect`]という
//! 別スレッドを立て、`TcpStream::peek`でそのソケットが閉じられていないかを
//! ポーリングする(`stream.try_clone()`で複製した、書き込みには使わない
//! 読み取り専用の複製)。`peek`が0バイトを返せば(相手がFIN/RSTを送った)、
//! そのセッションの[`crate::session::Session::cancellation_handle`]経由で
//! 実行中の文をキャンセルする。実行中の文が同期ポイント
//! (`crate::cancellation`モジュール冒頭を参照)へ到達すれば、そこで
//! `DbError::QueryCancelled`として打ち切られる。
//!
//! 明示的なキャンセル要求(クライアントが繋がったまま、別のメッセージで
//! 「今実行中の文をやめてほしい」と伝える経路)は、この章では追加しない。
//! PostgreSQLは専用の2本目のTCP接続とシークレットキーでこれを実現するが、
//! この教材のプロトコル(第36章)へ同じ仕組みを足すのは、接続の生成・
//! シークレットキーの受け渡しという新しい概念をこの章の範囲外にまで
//! 広げてしまう。この章では、[`crate::session::Session::cancellation_handle`]を
//! Rustの公開APIとして提供し(接続の切断検知も内部的にはこのAPIを呼ぶだけである)、
//! 埋め込み用途・テストコードは別スレッドから直接これを呼んでキャンセルできる
//! ようにするに留める(章末の演習課題を参照)。
//!
//! # Graceful Shutdown
//!
//! [`Server::shutdown_handle`]が返す[`ShutdownHandle`]の`trigger`を呼ぶと、
//! 次の順序で終了する。
//!
//! 1. **新規接続の受付停止**: [`Server::run`]の`accept`ループが、この
//!    フラグを見て抜ける。`TcpListener::accept`はブロッキング呼び出しの
//!    ままなので、フラグを立てただけでは`accept`の途中で眠っているスレッドは
//!    起きない。[`ShutdownHandle::trigger`]は、フラグを立てた直後に
//!    自分自身のアドレスへ`TcpStream::connect`する(**自己接続トリック**)。
//!    これにより`accept`がその接続を受理して1回だけ戻り、ループの先頭で
//!    フラグを確認して抜けられる。
//! 2. **実行中の文の完了(またはキャンセルによる打ち切り)**: 各接続の
//!    読み取りループは、次のフレームを待つ間`POLL_INTERVAL`ごとにこの
//!    フラグを確認しており([`wait_for_request_or_shutdown`])、フラグが
//!    立っていれば次のフレームを待たずに接続を終える。すでに実行中の文が
//!    あれば、その文が終わる(または[`crate::error::DbError::QueryTimeout`]・
//!    `QueryCancelled`で打ち切られる)まで待つ。[`crate::thread_pool::WorkerPool::join`]が、
//!    全ワーカースレッドがこの状態に達するまでブロックする。
//! 3. **未コミットTxのROLLBACK**: 各接続の`Session`がスコープを抜けるとき、
//!    `Drop`実装が保持中のトランザクションを`rollback_tx`する(前述)。
//! 4. **flush/sync**: `WorkerPool::join`が戻ってから(=すべての接続が
//!    後始末を終えてから)、[`Database::flush`]を呼ぶ。
//!
//! REPL(`src/main.rs`)の終了時flushと同じ「最後に1回`flush`する」という
//! 契約をサーバーでも守っている。REPLは`\q`・標準入力のEOFという単一スレッドの
//! 制御フローの終わりに直接`flush`を呼ぶだけだが、サーバーは複数の接続
//! スレッドの終了を`WorkerPool::join`で待ち合わせてから同じことをする点が
//! 異なる。
//!
//! ## 部分フレームを受信した接続の終了処理
//!
//! 手順2の「次のフレームを待つ間」は、まだ1バイトも届いていない接続だけの
//! 話ではない。クライアントがフレームのヘッダーだけ、あるいはペイロードの
//! 途中までしか送らずに止まった(接続は開いたまま)場合も、[`Server::run`]は
//! 同じ`POLL_INTERVAL`のうちに終了できなければならない。[`wait_for_request_or_shutdown`]は
//! `TcpStream::peek`で最初の1バイトが届くまでをポーリングするだけで、
//! 届いたあとの実際の読み取り([`Request::read`])はこの関数の外で行う。
//! [`Request::read`]自身は[`std::io::Read::read_exact`]を使うため、
//! 与えた`Read`実装がタイムアウトを返さない(無期限にブロックする)限り、
//! ヘッダーやペイロードの途中で相手が止まればそのまま戻ってこない。
//! [`ShutdownAwareReader`]は、`stream`の読み取りタイムアウトを
//! `POLL_INTERVAL`のまま外さずに`Request::read`へ渡すためのラッパーで、
//! タイムアウトのたびにシャットダウンフラグを確認し、立っていなければ
//! 同じ`read`を再試行する。これにより、フレームのどの位置で相手が止まって
//! いても、次の`POLL_INTERVAL`以内にシャットダウンへ気付ける。

use std::io::{ErrorKind, Read};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::cancellation::CancellationToken;
use crate::database::SharedDatabase;
use crate::protocol::{ProtocolError, Request, Response};
use crate::session::Session;
use crate::thread_pool::WorkerPool;

/// 接続の生死・シャットダウン要求を確認する間隔(第38章)。値が小さいほど
/// クライアント切断・Graceful Shutdownへの応答性が上がる代わりに、
/// 何もしていない接続をポーリングするコストが増える。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// [`Server::bind_with_config`]が受け取る設定。
#[derive(Debug, Clone, Copy)]
pub struct ServerConfig {
    /// 同時に接続を処理するワーカースレッドの本数
    /// (`crate::thread_pool`モジュール冒頭を参照)。
    pub workers: usize,
    /// ワーカーが全て塞がっているときに待たせる接続の上限。
    pub queue_capacity: usize,
}

impl Default for ServerConfig {
    /// ワーカー16本・キュー64、これを超える接続は拒否する
    /// (`crate::thread_pool`モジュール冒頭「上限を超えたらどうするか」を参照)。
    fn default() -> Self {
        ServerConfig { workers: 16, queue_capacity: 64 }
    }
}

/// TCP接続を受け付けるサーバー。
pub struct Server {
    listener: TcpListener,
    shared: Arc<SharedDatabase>,
    config: ServerConfig,
    shutdown: Arc<AtomicBool>,
}

/// [`Server::shutdown_handle`]が返す、Graceful Shutdownの起点(第38章)。
///
/// 実際のSIGINT(Ctrl-C)は`src/main.rs`が`ctrlc`クレートで受け取り、この
/// `trigger`を呼ぶだけの薄い橋渡しに留めている。この分離のおかげで、
/// テストコードはシグナルを実際に送る代わりに同じ`trigger`を直接呼んで
/// Graceful Shutdownの手順を検証できる(本文・テストを参照)。
pub struct ShutdownHandle {
    flag: Arc<AtomicBool>,
    local_addr: SocketAddr,
}

impl ShutdownHandle {
    /// Graceful Shutdownを開始する(モジュール冒頭の手順を参照)。何度
    /// 呼んでも安全(2回目以降は`connect`が失敗しても無視するだけ)。
    pub fn trigger(&self) {
        self.flag.store(true, Ordering::Release);
        // `accept`でブロック中のServer::runを起こす自己接続トリック
        // (モジュール冒頭を参照)。接続を受理させたいだけなので、返る
        // `TcpStream`はすぐdropしてよい。すでにlistenerが閉じていれば
        // (二重shutdown、またはrunがすでに戻っている)このconnectは
        // 失敗するが、その場合はもうaccept自体が残っていないので無視してよい。
        let _ = TcpStream::connect(self.local_addr);
    }
}

impl Server {
    /// `addr`にbindし、既定の[`ServerConfig`]で以後の接続が`shared`を共有する
    /// サーバーを作る。
    pub fn bind(addr: impl ToSocketAddrs, shared: Arc<SharedDatabase>) -> std::io::Result<Self> {
        Self::bind_with_config(addr, shared, ServerConfig::default())
    }

    /// [`Server::bind`]の、[`ServerConfig`]を指定できる版(第38章)。
    ///
    /// bindするだけで接続はまだ受け付けない([`Server::run`]が受け付ける)。
    /// テストがOS割り当てのポート(`"127.0.0.1:0"`)を使ってポート衝突による
    /// flakyさを避けられるよう、`bind`と`run`を分け、bind直後に
    /// [`Server::local_addr`]で実際に割り当てられたポートを取得できるようにしている。
    pub fn bind_with_config(addr: impl ToSocketAddrs, shared: Arc<SharedDatabase>, config: ServerConfig) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        Ok(Server { listener, shared, config, shutdown: Arc::new(AtomicBool::new(false)) })
    }

    /// 実際にbindされたアドレス(ポート0を指定した場合はOSが割り当てた実際の
    /// ポートを含む)を返す。
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// このサーバーのGraceful Shutdownを起点となる[`ShutdownHandle`]を返す
    /// (第38章)。[`Server::run`]を呼ぶ前でも後でも取得でき、`run`を実行している
    /// スレッドとは別のスレッド(シグナルハンドラ、テストコード)から
    /// `trigger`する。
    pub fn shutdown_handle(&self) -> ShutdownHandle {
        ShutdownHandle {
            flag: Arc::clone(&self.shutdown),
            local_addr: self.listener.local_addr().expect("Server::bindの時点でbind済み"),
        }
    }

    /// 接続を受け付け続ける。呼び出したスレッドをブロックする。
    ///
    /// [`ShutdownHandle::trigger`]が呼ばれるまで、接続を受け付けるたびに
    /// [`crate::thread_pool::WorkerPool`]へ渡す。プールが満杯なら、その接続は
    /// 拒否応答を書いてから切断する([`reject_connection`])。`trigger`が
    /// 呼ばれたら、モジュール冒頭のGraceful Shutdownの手順に従って
    /// `WorkerPool::join`・`SharedDatabase::flush`を行ってから戻る。
    pub fn run(self) -> std::io::Result<()> {
        let Server { listener, shared, config, shutdown } = self;
        let pool = {
            let shared = Arc::clone(&shared);
            let shutdown = Arc::clone(&shutdown);
            WorkerPool::new(config.workers, config.queue_capacity, move |stream| {
                handle_connection(stream, Arc::clone(&shared), Arc::clone(&shutdown));
            })
        };

        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if shutdown.load(Ordering::Acquire) {
                        // 自己接続トリック(`ShutdownHandle::trigger`)自身の接続、
                        // または受付停止の直前に滑り込んだ接続。どちらも処理せず
                        // 閉じる。
                        drop(stream);
                        break;
                    }
                    if let Err(rejected) = pool.dispatch(stream) {
                        reject_connection(rejected);
                    }
                }
                Err(err) => {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    return Err(err);
                }
            }
        }

        pool.join();
        shared.flush().map_err(std::io::Error::other)?;
        Ok(())
    }
}

/// ワーカープールが満杯だったため受け付けられなかった接続へ、エラー応答を
/// 1つ書いてから切断する(`crate::thread_pool`モジュール冒頭「上限を超えたら
/// どうするか」を参照)。まだ1件も`Request`を読んでいないので`request_id`は
/// `0`を使う(クライアント側が採番したどの`request_id`とも意味的に対応しない、
/// 接続そのものへのエラー)。
fn reject_connection(mut stream: TcpStream) {
    let response = Response::Error("接続数が上限に達しています。しばらくしてから再接続してください。".to_string());
    let _ = response.write(&mut stream, 0);
}

/// 1本のTCP接続を、切断されるまで処理する。
///
/// フレーミング自体が壊れているエラー(未知のメッセージ種別、上限超過の
/// フレーム長、ペイロードの途中でストリームが終わる等)を受け取ったら、
/// それ以上このストリームのバイト列を信用できない(`crate::protocol`
/// モジュール冒頭を参照)ため、エラー応答を試みることさえせず接続を切る。
fn handle_connection(mut stream: TcpStream, shared: Arc<SharedDatabase>, shutdown: Arc<AtomicBool>) {
    let mut session = Session::new(shared);
    loop {
        match wait_for_request_or_shutdown(&mut stream, &shutdown) {
            WaitOutcome::Shutdown | WaitOutcome::Disconnected => break,
            WaitOutcome::Ready => {}
        }

        // ヘッダー・ペイロードを読み終えるまで、`stream`の読み取り期限
        // (`POLL_INTERVAL`、`wait_for_request_or_shutdown`がすでに設定済み)を
        // 外さずに読む。フレームの途中(ヘッダーの途中、ペイロードの途中)で
        // 期限に達しても、`ShutdownAwareReader`がシャットダウンフラグを
        // 確認したうえで同じ`read`を再試行するため、フレーム境界がずれる
        // 心配は無い(モジュール冒頭「Graceful Shutdown」を参照)。
        let request = match Request::read(&mut ShutdownAwareReader { stream: &mut stream, shutdown: &shutdown }) {
            Ok(request) => request,
            Err(ProtocolError::Io(err)) if err.kind() == ErrorKind::UnexpectedEof => {
                // クライアントが次のフレームを送る前にソケットを閉じた
                // (正常な切断)。
                break;
            }
            Err(_) => break,
        };

        let cancel = session.cancellation_handle();
        let watcher = DisconnectWatcher::spawn(&stream, cancel);
        let response = Response::from_db_result(session.execute(&request.sql));
        if let Some(watcher) = watcher {
            watcher.stop();
        }

        if response.write(&mut stream, request.request_id).is_err() {
            // 応答を書き出せなかった(クライアントが読む前に切断した等)。
            // これ以上このストリームへ書いても仕方が無いので接続を終える。
            break;
        }
    }
    // `session`がここでdropされ、保持中のトランザクションがあればROLLBACK
    // される(`crate::session::Session`の`Drop`実装、モジュール冒頭を参照)。
}

enum WaitOutcome {
    /// 次のフレームの先頭バイトがすでに届いている。`stream`の読み取り
    /// タイムアウトは`POLL_INTERVAL`のままにしてある(モジュール冒頭
    /// 「部分フレームを受信した接続の終了処理」を参照)。
    Ready,
    /// シャットダウンが要求された。
    Shutdown,
    /// 相手が切断した(またはソケットが読めなくなった)。
    Disconnected,
}

/// 次のリクエストが届く(またはシャットダウン要求・切断)まで、`stream`を
/// ブロックしすぎずに待つ(モジュール冒頭「Graceful Shutdown」の手順2を参照)。
///
/// `TcpStream::peek`はバイト列を消費しない(`Request::read`が最初から
/// フレーム全体を読み直せる)ため、ポーリングのタイムアウトがフレームの
/// 到着前に発生しても、次のフレーム境界がずれる心配が無い。1バイト以上
/// 届いていることを確認できたら[`WaitOutcome::Ready`]を返すが、読み取り
/// タイムアウトは`None`(無期限)へは戻さない。ヘッダー・ペイロードの
/// 続きがまだ全部届いていない状態(部分フレーム)でも、後続の
/// `Request::read`(`ShutdownAwareReader`経由)が同じ`POLL_INTERVAL`の期限を
/// 使って読み進め、期限に達するたびにシャットダウンフラグを再確認できる
/// ようにするためである(モジュール冒頭「部分フレームを受信した接続の
/// 終了処理」を参照)。
fn wait_for_request_or_shutdown(stream: &mut TcpStream, shutdown: &AtomicBool) -> WaitOutcome {
    if stream.set_read_timeout(Some(POLL_INTERVAL)).is_err() {
        return WaitOutcome::Disconnected;
    }
    let mut probe = [0u8; 1];
    loop {
        if shutdown.load(Ordering::Acquire) {
            return WaitOutcome::Shutdown;
        }
        match stream.peek(&mut probe) {
            Ok(0) => return WaitOutcome::Disconnected, // 相手が正常に閉じた(EOF)
            Ok(_) => return WaitOutcome::Ready,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(_) => return WaitOutcome::Disconnected,
        }
    }
}

/// [`Request::read`]をシャットダウン要求に応答できる形でラップする
/// `Read`実装(モジュール冒頭「部分フレームを受信した接続の終了処理」を
/// 参照)。
///
/// `stream`の読み取りタイムアウトは、呼び出し元(`handle_connection`)が
/// `wait_for_request_or_shutdown`を通じてすでに`POLL_INTERVAL`へ設定済みで
/// あることを前提にする。`read`がタイムアウト(`WouldBlock`・`TimedOut`)を
/// 受け取るたびにシャットダウンフラグを確認し、立っていれば
/// (`Request::read`の内部で`read_exact`が積み上げている途中のバイト列ごと)
/// エラーとして中断する。立っていなければ同じ`read`を再試行する。
/// これにより、ヘッダーの途中・ペイロードの途中のどちらでシャットダウンが
/// 要求されても、次の`POLL_INTERVAL`以内に接続を終えられる。
struct ShutdownAwareReader<'a> {
    stream: &'a mut TcpStream,
    shutdown: &'a AtomicBool,
}

impl Read for ShutdownAwareReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            match self.stream.read(buf) {
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if self.shutdown.load(Ordering::Acquire) {
                        return Err(std::io::Error::other(
                            "シャットダウン要求によりフレームの読み取りを中断しました",
                        ));
                    }
                    continue;
                }
                other => return other,
            }
        }
    }
}

/// 文を1本実行している間、そのクライアントが切断していないかを別スレッドで
/// 監視する(モジュール冒頭「クライアント切断の検知」を参照)。
struct DisconnectWatcher {
    done: Arc<AtomicBool>,
    handle: thread::JoinHandle<()>,
}

impl DisconnectWatcher {
    /// `stream`を`try_clone`できた場合だけ監視スレッドを立てる。`try_clone`が
    /// 失敗する状況(OSのファイルディスクリプタ枯渇等)は稀であり、失敗しても
    /// 監視を諦めるだけでこの文自体は通常どおり実行を続けられる(縮退)。
    fn spawn(stream: &TcpStream, cancel: CancellationToken) -> Option<Self> {
        let watch_stream = stream.try_clone().ok()?;
        let done = Arc::new(AtomicBool::new(false));
        let done_for_thread = Arc::clone(&done);
        let handle = thread::spawn(move || watch_for_disconnect(watch_stream, &done_for_thread, &cancel));
        Some(DisconnectWatcher { done, handle })
    }

    /// 監視を止め、監視スレッドの終了を待つ。文の実行が(打ち切りではなく)
    /// 正常に終わった場合に呼ぶ。
    fn stop(self) {
        self.done.store(true, Ordering::Release);
        let _ = self.handle.join();
    }
}

/// [`DisconnectWatcher`]の監視スレッド本体。`done`が立つまで、`peek`で
/// ソケットの生死を`POLL_INTERVAL`ごとに確認する。切断を検知したら`cancel`する。
fn watch_for_disconnect(stream: TcpStream, done: &AtomicBool, cancel: &CancellationToken) {
    if stream.set_read_timeout(Some(POLL_INTERVAL)).is_err() {
        return;
    }
    let mut probe = [0u8; 1];
    loop {
        if done.load(Ordering::Acquire) {
            return;
        }
        match stream.peek(&mut probe) {
            Ok(0) => {
                cancel.cancel();
                return;
            }
            // 1バイト以上読めた場合は切断ではない。パイプライン化されたクライアント
            // (次のリクエストを先読みで送ってくる)を誤ってキャンセルしないよう、
            // データの中身は見ず読み進めもせずにポーリングを続ける。
            Ok(_) => continue,
            Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(_) => {
                cancel.cancel();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::protocol::{MSG_QUERY, Request, Response};
    use std::io::Write;
    use std::net::TcpStream;

    fn connect_with_retry(addr: SocketAddr) -> TcpStream {
        for _ in 0..50 {
            if let Ok(stream) = TcpStream::connect(addr) {
                return stream;
            }
            thread::sleep(Duration::from_millis(10));
        }
        TcpStream::connect(addr).expect("接続に失敗しました")
    }

    fn send_sql(stream: &mut TcpStream, request_id: u32, sql: &str) -> Response {
        Request { request_id, sql: sql.to_string() }.write(stream).expect("送信に失敗しました");
        let (received, response) = Response::read(stream).expect("受信に失敗しました");
        assert_eq!(received, request_id);
        response
    }

    /// [`send_sql`]と違い、拒否応答(`request_id`が`0`固定、`reject_connection`の
    /// ドキュメント参照)が返っても`request_id`の一致を確認しない。ワーカー
    /// プールが満杯かどうかを問い合わせるテストで使う。
    fn send_sql_ignoring_request_id(stream: &mut TcpStream, request_id: u32, sql: &str) -> Response {
        Request { request_id, sql: sql.to_string() }.write(stream).ok();
        Response::read(stream).expect("受信に失敗しました").1
    }

    /// N+1本目の接続の挙動: ワーカー1本・キュー容量0のサーバーでは、1本の
    /// 接続を張っているだけでその唯一のワーカーを占有し続ける
    /// (`handle_connection`は次のリクエストを待つ間もワーカースレッドの中に
    /// 居続けるため、実行中のクエリが無くても"空き"には戻らない)。その間に
    /// 来た2本目の接続は、キューに空きが無いため即座に拒否応答を受け取る
    /// (`crate::thread_pool`モジュール冒頭「上限を超えたらどうするか」)。
    /// 1本目が切断すれば、3本目は通常どおり処理される。
    #[test]
    fn the_n_plus_first_connection_is_rejected_while_the_single_worker_is_busy() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let server = Server::bind_with_config("127.0.0.1:0", shared, ServerConfig { workers: 1, queue_capacity: 0 }).unwrap();
        let addr = server.local_addr().unwrap();
        let shutdown = server.shutdown_handle();
        thread::spawn(move || server.run().unwrap());

        // 1本目: 1往復させ、ワーカーが確かにこの接続のハンドラの中にいることを
        // 確認する。サーバースレッドが立ち上がってワーカーが`recv`で待ち始める
        // までのわずかな時間は、この接続自身も(空いているワーカーが無いという
        // 意味で)拒否されうるため、拒否されたら新しい接続でリトライする。
        let mut first = connect_with_retry(addr);
        let mut first_response = send_sql_ignoring_request_id(&mut first, 1, "SELECT 1");
        for _ in 0..100 {
            if matches!(first_response, Response::Rows { .. }) {
                break;
            }
            first = connect_with_retry(addr);
            first_response = send_sql_ignoring_request_id(&mut first, 1, "SELECT 1");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(first_response, Response::Rows { .. }), "ワーカーが起動していれば1本目は受理されるはず");

        // ワーカーは1本(queue_capacity=0)。`first`のハンドラは次のリクエストを
        // 待っている間もワーカースレッドを離れないため、2本目の接続の`dispatch`は
        // キューに空きが無く即座に失敗し、`reject_connection`が拒否応答を書く。
        // まだ`Request`を1つも読んでいない拒否応答の`request_id`は`0`固定
        // (`reject_connection`のドキュメント参照)なので、ここでは`request_id`の
        // 一致は確認せず`Response`の種別だけを見る。
        let mut second = connect_with_retry(addr);
        Request { request_id: 2, sql: "SELECT 1".to_string() }.write(&mut second).ok();
        let (_, response) = Response::read(&mut second).expect("拒否応答を受信できませんでした");
        assert!(matches!(response, Response::Error(_)), "唯一のワーカーが1本目の接続に占有されている間、2本目は拒否されるはず");
        drop(second);

        drop(first);
        // 1本目が切れれば(`wait_for_request_or_shutdown`がPOLL_INTERVALごとに
        // 検知する)ワーカーが空く。検知までの猶予はsleepで決め打ちせず、
        // 「まだ拒否される」応答が返る間はリトライする形で待つ。
        let mut third_result = None;
        for _ in 0..100 {
            let mut third = connect_with_retry(addr);
            let response = send_sql_ignoring_request_id(&mut third, 3, "SELECT 1");
            if matches!(response, Response::Rows { .. }) {
                third_result = Some(response);
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(third_result, Some(Response::Rows { .. })), "1本目が切断した後の3本目は通常どおり処理されるはず");

        shutdown.trigger();
    }

    /// Graceful Shutdown: シグナルの代わりに`ShutdownHandle::trigger`(内部API)を
    /// 呼び、(a)未コミットのトランザクションがROLLBACKされ、(b)`run`が戻り、
    /// (c)flush済みでファイルを再オープンできることを確認する。
    #[test]
    fn graceful_shutdown_rolls_back_and_flushes_before_run_returns() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "minidb-server-shutdown-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let db = Database::open(&path).unwrap();
        let shared = Arc::new(SharedDatabase::new(db));
        let server = Server::bind("127.0.0.1:0", shared).unwrap();
        let addr = server.local_addr().unwrap();
        let shutdown = server.shutdown_handle();
        let run_handle = thread::spawn(move || server.run());

        let mut stream = connect_with_retry(addr);
        assert!(matches!(send_sql(&mut stream, 1, "CREATE TABLE t (id BIGINT)"), Response::Command(_)));
        assert!(matches!(send_sql(&mut stream, 2, "BEGIN"), Response::Command(_)));
        assert!(matches!(send_sql(&mut stream, 3, "INSERT INTO t VALUES (1)"), Response::Command(_)));
        // COMMITしないまま、接続を保持したままshutdownする。

        shutdown.trigger();
        run_handle.join().expect("runがpanicした").expect("runがErrを返した");

        // 再オープンして、コミットしていなかったINSERTが残っていないこと
        // (ROLLBACKされたこと)と、`CREATE TABLE`自体はflushされていることを
        // 確認する。
        drop(stream);
        let reopened = Database::open(&path).unwrap();
        let mut session = crate::session::Session::new(Arc::new(SharedDatabase::new(reopened)));
        let result = session.execute("SELECT * FROM t").unwrap();
        assert!(result.rows().is_empty(), "COMMITしていないINSERTはROLLBACKされ、再オープン後も残らないはず");

        std::fs::remove_file(&path).unwrap();
    }

    /// `run`をバックグラウンドスレッドで動かし、その結果を`mpsc`チャネルへ
    /// 送る。`JoinHandle::join`は無期限にブロックしうるため、テストからは
    /// `channel`の`recv_timeout`で「`POLL_INTERVAL`の数サイクル以内に確実に
    /// 戻ってくる」ことを検証する(戻ってこなければ`recv_timeout`が
    /// `Err`になり、テストがハングする代わりに失敗する)。
    fn run_in_background(server: Server) -> std::sync::mpsc::Receiver<std::io::Result<()>> {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(server.run());
        });
        rx
    }

    /// 第6部レビュー対応: フレームのヘッダー9バイトのうち3バイトだけ送って
    /// 接続を開いたまま止め、その状態からシャットダウンを要求する。
    ///
    /// 修正前は、最初の1バイトが届いた時点で`wait_for_request_or_shutdown`が
    /// 読み取りタイムアウトを`None`(無期限)へ戻していたため、`Request::read`の
    /// `read_exact`が残り6バイトを待ったまま戻らず、`Server::run`が
    /// `WorkerPool::join`から永久に戻れなかった。
    #[test]
    fn shutdown_completes_while_a_connection_is_stalled_mid_header() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let server = Server::bind_with_config("127.0.0.1:0", shared, ServerConfig::default()).unwrap();
        let addr = server.local_addr().unwrap();
        let shutdown = server.shutdown_handle();
        let rx = run_in_background(server);

        let mut stalled = connect_with_retry(addr);
        // 9バイトのヘッダーのうち3バイトだけ送る。残りは送らず、接続も
        // 閉じない。
        stalled.write_all(&[MSG_QUERY, 0x00, 0x00]).unwrap();

        shutdown.trigger();
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("shutdownがPOLL_INTERVALの数サイクル以内に完了しませんでした(部分ヘッダー状態で接続がstallしたまま)");
        result.expect("runがErrを返した");
        drop(stalled);
    }

    /// 第6部レビュー対応: [`shutdown_completes_while_a_connection_is_stalled_mid_header`]と
    /// 対になる、ペイロードの途中で止まった接続からのシャットダウン。
    /// ヘッダーは完全に送り、`payload_len`で100バイトを申告したうえで
    /// 実際には10バイトしか送らず、接続を開いたまま止める。
    #[test]
    fn shutdown_completes_while_a_connection_is_stalled_mid_payload() {
        let shared = Arc::new(SharedDatabase::new(Database::memory()));
        let server = Server::bind_with_config("127.0.0.1:0", shared, ServerConfig::default()).unwrap();
        let addr = server.local_addr().unwrap();
        let shutdown = server.shutdown_handle();
        let rx = run_in_background(server);

        let mut stalled = connect_with_retry(addr);
        let mut header = Vec::new();
        header.push(MSG_QUERY);
        header.extend_from_slice(&0u32.to_le_bytes()); // request_id
        header.extend_from_slice(&100u32.to_le_bytes()); // payload_len(100バイトと申告)
        stalled.write_all(&header).unwrap();
        stalled.write_all(&[b'x'; 10]).unwrap(); // 実際には10バイトしか送らない

        shutdown.trigger();
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("shutdownがPOLL_INTERVALの数サイクル以内に完了しませんでした(部分ペイロード状態で接続がstallしたまま)");
        result.expect("runがErrを返した");
        drop(stalled);
    }
}
