//! minidbのCLIクライアント(第36章)。
//!
//! `src/main.rs`のREPLと対話UIは同じで、SQLの実行先だけが違う。REPLは
//! `Database::execute`をプロセス内で直接呼ぶが、このクライアントは
//! `crate::protocol::Request`/`Response`をTCP接続越しに送受信し、実際の
//! SQL実行は別プロセス(`cargo run -- --serve <addr> <db-path>`で起動した
//! サーバー)の`SharedDatabase`が行う。CLIクライアントとサーバーが同じ
//! `SharedDatabase`経由の実行経路を通るという構造は`src/server.rs`の
//! `Session`が担っており、このバイナリの役割は「標準入力から読んだSQLを
//! フレームに詰めて送り、返ってきたフレームを表示する」という入出力の
//! 皮だけである。
//!
//! # 使い方
//!
//! ```text
//! cargo run --bin minidb-client -- 127.0.0.1:5432
//! ```

use std::fmt;
use std::io::{self, BufRead, Write};
use std::net::TcpStream;

use minidb::protocol::{Request, Response};

fn main() {
    let mut args = std::env::args();
    let _program_name = args.next();
    let Some(addr) = args.next() else {
        eprintln!("使い方: minidb-client <addr>  (例: minidb-client 127.0.0.1:5432)");
        std::process::exit(1);
    };

    let mut stream = match TcpStream::connect(&addr) {
        Ok(stream) => stream,
        Err(e) => {
            eprintln!("エラー: {addr}へ接続できませんでした: {e}");
            std::process::exit(1);
        }
    };

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut next_request_id: u32 = 1;

    prompt(&mut stdout);
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let input = line.trim();

        if input.is_empty() {
            prompt(&mut stdout);
            continue;
        }
        if input == "\\q" {
            break;
        }

        match send(&mut stream, next_request_id, input) {
            Ok(response) => println!("{response}"),
            Err(e) => {
                // フレームそのものが読み書きできなかった、または応答の
                // `request_id`が送信した`request_id`と一致しなかった(`ClientError`の
                // ドキュメント参照)。個別のクエリのエラー(`Response::Error`)とは
                // 異なり、これ以上このクライアントで対話を続けられないので終了する。
                eprintln!("エラー: サーバーとの通信に失敗しました: {e}");
                return;
            }
        }
        next_request_id = next_request_id.wrapping_add(1);
        prompt(&mut stdout);
    }
}

/// `send`が返しうるエラー。フレームそのものが読み書きできなかった
/// [`minidb::ProtocolError`]に、この章のクライアント自身が検査する
/// `request_id`の不一致を追加した型。
#[derive(Debug)]
enum ClientError {
    Protocol(minidb::ProtocolError),
    /// 応答の`request_id`が、送信したリクエストの`request_id`と一致しなかった
    /// (第6部レビュー対応)。この章のサーバーは1本の接続の中でリクエストを
    /// 1件ずつ順に処理するため、本来は起こらないはずの状態である。それでも
    /// 検査せずに応答をそのまま表示すると、フレームの境界がどこかでずれて
    /// いた場合に、別のリクエストの結果を気付かず表示してしまう
    /// (`crate::protocol`モジュール冒頭「request_id」を参照)。この章はこの
    /// 不一致を検出したら通信エラーとして接続を終了するに留め、
    /// 再送や自動的な補正は行わない。
    RequestIdMismatch { sent: u32, received: u32 },
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClientError::Protocol(err) => write!(f, "{err}"),
            ClientError::RequestIdMismatch { sent, received } => {
                write!(f, "応答のrequest_idが一致しません: 送信は{sent}でしたが受信は{received}でした")
            }
        }
    }
}

impl From<minidb::ProtocolError> for ClientError {
    fn from(err: minidb::ProtocolError) -> Self {
        ClientError::Protocol(err)
    }
}

/// `sql`を1件送り、応答を受け取る。受信した`request_id`が送信した
/// `request_id`と一致することを検証してから[`Response`]を返す
/// (`ClientError::RequestIdMismatch`のドキュメント参照)。
fn send(stream: &mut TcpStream, request_id: u32, sql: &str) -> Result<Response, ClientError> {
    let request = Request { request_id, sql: sql.to_string() };
    request.write(stream)?;
    let (received_id, response) = Response::read(stream)?;
    if received_id != request_id {
        return Err(ClientError::RequestIdMismatch { sent: request_id, received: received_id });
    }
    Ok(response)
}

/// プロンプトを表示する。バッファリングされた標準出力を即座に流すため`flush`する。
fn prompt(stdout: &mut io::Stdout) {
    print!("minidb> ");
    stdout.flush().ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// クライアントの`Request`を1つ読み捨て、`response_request_id`を
    /// `request_id`として持つ`STATUS_OK_COMMAND`応答を1つ書き返すだけの、
    /// テスト専用の最小サーバー。
    fn spawn_responder(response_request_id: u32) -> std::net::SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("接続を受理できませんでした");
            Request::read(&mut stream).expect("リクエストを読めませんでした");
            Response::Command("OK".to_string())
                .write(&mut stream, response_request_id)
                .expect("応答を書けませんでした");
        });
        addr
    }

    #[test]
    fn send_accepts_a_response_whose_request_id_matches_the_sent_one() {
        let addr = spawn_responder(42);
        let mut stream = TcpStream::connect(addr).expect("接続に失敗しました");
        let response = send(&mut stream, 42, "SELECT 1").expect("request_idが一致するので成功するはず");
        assert!(matches!(response, Response::Command(_)));
    }

    #[test]
    fn send_rejects_a_response_whose_request_id_does_not_match_the_sent_one() {
        let addr = spawn_responder(999);
        let mut stream = TcpStream::connect(addr).expect("接続に失敗しました");
        let err = send(&mut stream, 42, "SELECT 1").expect_err("request_idが一致しないので失敗するはず");
        assert!(matches!(err, ClientError::RequestIdMismatch { sent: 42, received: 999 }), "{err:?}");
    }
}
