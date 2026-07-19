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
//! # 接続単位のトランザクション状態: [`Session`]
//!
//! 第30章以来、`Database`が同時に持てる通常のSQL経路のトランザクションは
//! 高々1本だった(`self.tx: Option<TransactionContext>`)。複数の接続が同時に
//! `BEGIN`していれば、この制約はもう成り立たない。この章では、`Database`の
//! 内部を書き換える代わりに、決定的インターリーブテストハーネス(第30章)が
//! すでに用意していた`begin_tx`/`execute_in_tx`/`commit_tx`/`rollback_tx`という
//! `TxHandle`ベースの経路を、接続ごとに1個ずつ使う。
//!
//! `SharedDatabase::execute_in_tx`は、`Database::execute`の先頭で行っている
//! `BEGIN`・`COMMIT`・`ROLLBACK`の分岐(`Statement::Begin`等)を経由しない
//! (`Database::execute_in_tx`のドキュメント参照)。そのため[`Session`]は、
//! クライアントから届いたSQLを一度パースし、`BEGIN`・`COMMIT`・`ROLLBACK`
//! だけを`SharedDatabase`の`TxHandle`API(`begin_tx_with_isolation`・
//! `commit_tx`・`rollback_tx`)へ翻訳してから、残りの文だけを`execute_in_tx`へ
//! 渡す。この翻訳は`Database::execute`の先頭の分岐とほぼ同じ形をしており、
//! 二重管理に見えるかもしれない。接続ごとのトランザクション状態を`Database`
//! 自身にではなく接続の側(このモジュール)に持たせる設計を選んだ以上、
//! `BEGIN`等をどちらの状態(`Database::tx`か、この`Session::tx`か)へ反映するかの
//! 判断も、状態を持つ側で行う必要がある。この重複を「セッションの状態と
//! 責務を1箇所にまとめる」形で解消するのが第37章の仕事であり、この章では
//! 接続ごとに独立したトランザクション状態を持てるという最小限の性質だけを
//! 満たす前身にとどめる。
//!
//! 明示的な`BEGIN`が無いまま届いた文(Autocommit)は、`begin_tx`で1本だけの
//! トランザクションを開き、成功したら`commit_tx`、失敗したら`rollback_tx`する
//! ことで、1文だけのトランザクションとして実行する。`Database::execute`の
//! Autocommit(`self.tx`が`None`のときに`lock_owner`が文ごとの`TransactionId`を
//! 割り当てる経路)と、結果として得られる直列化の単位は同じである。
//!
//! `CHECKPOINT`は`SharedDatabase`が`TxHandle`API越しに公開していない
//! (`Database::execute_checkpoint`はトランザクション境界の外側の操作であり、
//! `harness_contexts`のAPIに乗らない)。この章のサーバー経由では未対応とし、
//! クライアントへは`DbError::NotImplemented`をエラー応答として返す。
//!
//! # 切断時のトランザクション後始末
//!
//! 接続が(正常な`\q`ではなく)途中で切れた場合、そのセッションが持っていた
//! 未コミットのトランザクションをどうするかを決めておかないと、そのトランザクション
//! が獲得したロック(第31章)を他の接続が永久に待たされる。この章では
//! [`Session`]の`Drop`実装で、保持中の`TxHandle`があれば無条件に
//! `rollback_tx`する。読み取り専用の接続がソケットエラーで終了した場合も
//! `rollback_tx`を呼ぶが、対象のトランザクションはロックを1つも保持していない
//! ことがあり、その場合は何もしないのと実質的に同じである。`Session`の`Drop`は
//! 正常な切断・異常な切断・パニックのどの経路でも通るため、後始末を
//! 呼び出し側の分岐(切断理由ごとの`match`)に分散させずに1箇所へ集約できる。
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

use crate::ast::Statement;
use crate::database::{QueryResult, SharedDatabase, TxHandle};
use crate::error::{DbError, DbResult};
use crate::protocol::{ProtocolError, Request, Response};

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
                handle_connection(stream, &shared);
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
fn handle_connection(mut stream: TcpStream, shared: &SharedDatabase) {
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

        let response = session.execute(&request.sql);
        if response.write(&mut stream, request.request_id).is_err() {
            // 応答を書き出せなかった(クライアントが読む前に切断した等)。
            // これ以上このストリームへ書いても仕方が無いので接続を終える。
            break;
        }
    }
    // `session`がここでdropされ、保持中のトランザクションがあればROLLBACK
    // される(`Session`の`Drop`実装、モジュール冒頭を参照)。
}

/// 1本の接続が持つ、接続単位のトランザクション状態。
///
/// この章の時点では、保持するのは「今アクティブな`TxHandle`」だけである。
/// より本格的なセッション状態(Prepared Statement、パラメータ束縛等)は
/// 第37章が追加する。
struct Session<'a> {
    shared: &'a SharedDatabase,
    tx: Option<TxHandle>,
}

impl<'a> Session<'a> {
    fn new(shared: &'a SharedDatabase) -> Self {
        Session { shared, tx: None }
    }

    /// `sql`を1文実行し、クライアントへ送るための[`Response`]を組み立てる。
    ///
    /// `BEGIN`・`COMMIT`・`ROLLBACK`はこのセッションの`tx`を書き換える
    /// 特別な文として扱い、それ以外の文は`self.tx`があれば`execute_in_tx`、
    /// 無ければAutocommit(1文だけの`begin_tx`→`execute_in_tx`→
    /// `commit_tx`/`rollback_tx`)として実行する(モジュール冒頭を参照)。
    fn execute(&mut self, sql: &str) -> Response {
        Response::from_db_result(self.execute_inner(sql))
    }

    fn execute_inner(&mut self, sql: &str) -> DbResult<QueryResult> {
        let statement = crate::parser::parse_statement(sql)?;
        match statement {
            Statement::Begin(begin) => {
                if self.tx.is_some() {
                    return Err(DbError::TransactionAlreadyActive);
                }
                let level = begin
                    .isolation_level
                    .unwrap_or(crate::ast::IsolationLevel::RepeatableRead);
                self.tx = Some(self.shared.begin_tx_with_isolation(level));
                Ok(QueryResult::command("BEGIN"))
            }
            Statement::Commit(_) => {
                let handle = self.tx.take().ok_or(DbError::NoActiveTransaction)?;
                self.shared.commit_tx(handle)?;
                Ok(QueryResult::command("COMMIT"))
            }
            Statement::Rollback(_) => {
                let handle = self.tx.take().ok_or(DbError::NoActiveTransaction)?;
                self.shared.rollback_tx(handle)?;
                Ok(QueryResult::command("ROLLBACK"))
            }
            Statement::Checkpoint(_) => Err(DbError::NotImplemented(
                "CHECKPOINTはWire Protocol経由のセッションでは未対応です(第36章の範囲外)".to_string(),
            )),
            _ => match &self.tx {
                Some(handle) => self.shared.execute_in_tx(handle, sql),
                None => self.execute_autocommit(sql),
            },
        }
    }

    /// 明示的な`BEGIN`の外で届いた1文を、それ専用の`TxHandle`で実行する。
    /// 成功すればすぐ`commit_tx`、失敗すれば`rollback_tx`し、どちらの場合も
    /// この文の実行が終わった時点でロックを持ち越さない
    /// (`Database::execute`のAutocommit経路と同じ直列化の単位、モジュール
    /// 冒頭を参照)。
    fn execute_autocommit(&self, sql: &str) -> DbResult<QueryResult> {
        let handle = self.shared.begin_tx();
        match self.shared.execute_in_tx(&handle, sql) {
            Ok(result) => {
                self.shared.commit_tx(handle)?;
                Ok(result)
            }
            Err(err) => {
                // `rollback_tx`自体の失敗より、元の実行エラーの方が呼び出し
                // 元にとって重要な情報なので、`rollback_tx`のエラーは握り
                // つぶす。
                let _ = self.shared.rollback_tx(handle);
                Err(err)
            }
        }
    }
}

impl Drop for Session<'_> {
    /// 接続が切れた時点で未コミットのトランザクションが残っていれば
    /// ROLLBACKする(モジュール冒頭「切断時のトランザクション後始末」を参照)。
    fn drop(&mut self) {
        if let Some(handle) = self.tx.take() {
            let _ = self.shared.rollback_tx(handle);
        }
    }
}
