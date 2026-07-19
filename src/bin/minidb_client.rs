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
                // フレームそのものが読み書きできなかった(接続が切れた等)。
                // 個別のクエリのエラー(`Response::Error`)とは異なり、これ以上
                // このクライアントで対話を続けられないので終了する。
                eprintln!("エラー: サーバーとの通信に失敗しました: {e}");
                return;
            }
        }
        next_request_id = next_request_id.wrapping_add(1);
        prompt(&mut stdout);
    }
}

/// `sql`を1件送り、応答を受け取る。
fn send(stream: &mut TcpStream, request_id: u32, sql: &str) -> Result<Response, minidb::ProtocolError> {
    let request = Request { request_id, sql: sql.to_string() };
    request.write(stream)?;
    let (_received_id, response) = Response::read(stream)?;
    Ok(response)
}

/// プロンプトを表示する。バッファリングされた標準出力を即座に流すため`flush`する。
fn prompt(stdout: &mut io::Stdout) {
    print!("minidb> ");
    stdout.flush().ok();
}
