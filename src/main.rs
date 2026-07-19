//! minidbのREPL、および`--serve`によるサーバー起動(第36章)。
//!
//! # REPLモード(既定)
//!
//! 標準入力からSQLを1行ずつ読み、`crate::session::Session::execute`(第37章)に
//! 渡して結果を表示する。`\q`を入力すると終了する。REPLは`Database`を直接
//! 保持せず、`SharedDatabase`を1個包んだ`Session`を1個だけ作って使い回す
//! (`minidb::session`モジュールのドキュメント「Embedded・REPL・Serverの統一」
//! を参照)。これにより、REPLでも`PREPARE`・`EXECUTE`・`DEALLOCATE`が使える。
//!
//! 起動引数にファイルパスを渡すと、そのパスを`Database::open`(第16章)で開き、
//! 永続モードで動く(`cargo run -- example.db`)。引数を渡さなければ、これまで
//! どおり`Database::memory`のインメモリモードで動く。永続モードで終了すると
//! きは、`\q`の入力でも標準入力のEOFでも、抜ける前に必ず`Database::flush`を
//! 呼び、キャッシュされた変更をファイルへ書き戻す。
//!
//! # サーバーモード(`--serve`)
//!
//! 先頭の引数に`--serve <addr>`(永続モードなら続けて`<db-path>`)を渡すと、
//! REPLの代わりに`minidb::Server`をbindし、接続を受け付け続ける
//! (`cargo run -- --serve 127.0.0.1:5432 example.db`)。`<db-path>`を省略すると
//! インメモリモードでサーバーを起動する。サーバーモードには`\q`に相当する
//! 終了操作が無く、プロセスの終了(Ctrl-C等)でしか止まらない
//! (Graceful Shutdownは第38章、`crate::server`のモジュールドキュメント参照)。
//! `cargo run --bin minidb-client -- <addr>`で接続すると、REPLと同じ対話UIで
//! SQLを実行できる(`src/bin/minidb_client.rs`)。

use std::env;
use std::io::{self, BufRead, Write};
use std::sync::Arc;

use minidb::{Database, Server, SharedDatabase, Session};

fn main() {
    let mut args = env::args();
    let _program_name = args.next();

    match args.next() {
        Some(flag) if flag == "--serve" => run_server(args),
        first_arg => run_repl(first_arg),
    }
}

/// `--serve <addr> [db-path]`を処理する。
fn run_server(mut args: env::Args) {
    let Some(addr) = args.next() else {
        eprintln!("使い方: minidb --serve <addr> [db-path]");
        std::process::exit(1);
    };
    let db = match args.next() {
        Some(path) => match Database::open(&path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("エラー: {path}を開けませんでした: {e}");
                return;
            }
        },
        None => Database::memory(),
    };

    let shared = Arc::new(SharedDatabase::new(db));
    let server = match Server::bind(&addr, shared) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("エラー: {addr}へbindできませんでした: {e}");
            return;
        }
    };
    println!("minidb: {addr}で接続を待機しています");
    if let Err(e) = server.run() {
        eprintln!("エラー: サーバーが終了しました: {e}");
    }
}

/// REPLモードを処理する。`first_arg`は起動引数の先頭(`--serve`ではないと
/// すでに確認済み)で、`Some`ならファイルパスとして永続モードを開く。
fn run_repl(first_arg: Option<String>) {
    let db = match first_arg {
        Some(path) => match Database::open(&path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("エラー: {path}を開けませんでした: {e}");
                return;
            }
        },
        None => Database::memory(),
    };
    let shared = Arc::new(SharedDatabase::new(db));
    let mut session = Session::new(Arc::clone(&shared));

    let stdin = io::stdin();
    let mut stdout = io::stdout();

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

        match session.execute(input) {
            Ok(result) => println!("{result}"),
            Err(e) => println!("エラー: {e}"),
        }
        prompt(&mut stdout);
    }

    if let Err(e) = shared.flush() {
        eprintln!("エラー: 終了時のflushに失敗しました: {e}");
    }
}

/// プロンプトを表示する。バッファリングされた標準出力を即座に流すため`flush`する。
fn prompt(stdout: &mut io::Stdout) {
    print!("minidb> ");
    stdout.flush().ok();
}
