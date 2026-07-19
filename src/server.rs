//! Blocking I/OのTCPサーバー(第36章)。
//!
//! `crate::protocol`が定義するフレームを使って、複数のTCP接続を受け付け、
//! `crate::database::SharedDatabase`(第35章)を経由してSQLを実行する。
//!
//! # 接続ごとに1本のOSスレッド
//!
//! この章では、接続を受け付けるたびに`std::thread::spawn`でスレッドを1本
//! 立てる、最も単純な並行モデルを採る。接続数が増えるとスレッド数がそのまま
//! 増え続けるため、大量の接続を長時間張られる用途には向かない。
//! スレッドプールで接続をワーカースレッドの集合へ束ねる設計は第38章(実行制御)
//! に譲り、この章では「複数のクライアントプロセスが同時にこのサーバーへ
//! つながる」という第35章まで無かった性質だけを、最短の実装で確立する。
//!
//! `SharedDatabase`はすでに`Mutex`1本でスレッド間排他を行っている(第35章)ため、
//! この章のサーバー自身がロックを追加で管理する必要は無い。複数の接続スレッドが
//! 同時にSQLを送ってきても、`SharedDatabase`の内側で直列化される。
//!
//! # 接続単位の状態: [`crate::session::Session`]
//!
//! 接続ごとのトランザクション状態・Prepared Statementの名前空間は
//! [`crate::session::Session`](第37章)が一元管理する。このモジュールの
//! 役目は、TCP接続を受け付けて`Session`を1個作り、フレームを読んでは
//! `Session::execute`へ渡し、返ってきた`QueryResult`をフレームへ書き戻す
//! ことだけである。第36章の時点ではこのモジュール自身が接続ごとの
//! トランザクション状態を持つ`Session`という前身を実装していたが、
//! Embedded・REPL・Serverの3経路すべてで同じ状態管理を使うために
//! `crate::session`へ引き上げた(`crate::session`モジュールのドキュメント
//! 「Embedded・REPL・Serverの統一」を参照)。
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
//! # Graceful Shutdownの範囲外
//!
//! [`Server::run`]は`TcpListener::incoming`を無限にループするだけで、
//! 外部からの停止要求を受け付ける仕組みを持たない。実行中の接続を待ってから
//! 止める・新規接続の受付だけを先に止める、といったGraceful Shutdownは
//! 第38章の範囲であり、この章では単純にプロセスを終了させる(Ctrl-C等)ことで
//! 止める。

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::thread;

use crate::database::SharedDatabase;
use crate::protocol::{ProtocolError, Request, Response};
use crate::session::Session;

/// TCP接続を受け付けるサーバー。
pub struct Server {
    listener: TcpListener,
    shared: Arc<SharedDatabase>,
}

impl Server {
    /// `addr`にbindし、以後の接続が`shared`を共有するサーバーを作る。
    ///
    /// bindするだけで接続はまだ受け付けない([`Server::run`]が受け付ける)。
    /// テストがOS割り当てのポート(`"127.0.0.1:0"`)を使ってポート衝突による
    /// flakyさを避けられるよう、`bind`と`run`を分け、bind直後に
    /// [`Server::local_addr`]で実際に割り当てられたポートを取得できるようにしている。
    pub fn bind(addr: impl ToSocketAddrs, shared: Arc<SharedDatabase>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        Ok(Server { listener, shared })
    }

    /// 実際にbindされたアドレス(ポート0を指定した場合はOSが割り当てた実際の
    /// ポートを含む)を返す。
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// 接続を受け付け続ける。呼び出したスレッドをブロックする。
    ///
    /// 接続を受け付けるたびに新しいスレッドを立てて[`handle_connection`]へ渡し、
    /// このスレッド自身はすぐ次の`accept`へ戻る(モジュール冒頭「接続ごとに
    /// 1本のOSスレッド」を参照)。
    pub fn run(self) -> std::io::Result<()> {
        for stream in self.listener.incoming() {
            let stream = stream?;
            let shared = Arc::clone(&self.shared);
            thread::spawn(move || {
                handle_connection(stream, shared);
            });
        }
        Ok(())
    }
}

/// 1本のTCP接続を、切断されるまで処理する。
///
/// フレーミング自体が壊れているエラー(未知のメッセージ種別、上限超過の
/// フレーム長、ペイロードの途中でストリームが終わる等)を受け取ったら、
/// それ以上このストリームのバイト列を信用できない(`crate::protocol`
/// モジュール冒頭を参照)ため、エラー応答を試みることさえせず接続を切る。
fn handle_connection(mut stream: TcpStream, shared: Arc<SharedDatabase>) {
    let mut session = Session::new(shared);
    loop {
        let request = match Request::read(&mut stream) {
            Ok(request) => request,
            Err(ProtocolError::Io(err)) if err.kind() == ErrorKind::UnexpectedEof => {
                // クライアントが次のフレームを送る前にソケットを閉じた
                // (正常な切断)。
                break;
            }
            Err(_) => break,
        };

        let response = Response::from_db_result(session.execute(&request.sql));
        if response.write(&mut stream, request.request_id).is_err() {
            // 応答を書き出せなかった(クライアントが読む前に切断した等)。
            // これ以上このストリームへ書いても仕方が無いので接続を終える。
            break;
        }
    }
    // `session`がここでdropされ、保持中のトランザクションがあればROLLBACK
    // される(`crate::session::Session`の`Drop`実装、モジュール冒頭を参照)。
}
