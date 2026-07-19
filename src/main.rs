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
//! インメモリモードでサーバーを起動する。`cargo run --bin minidb-client -- <addr>`で
//! 接続すると、REPLと同じ対話UIでSQLを実行できる(`src/bin/minidb_client.rs`)。
//!
//! ワーカースレッド数・接続キュー容量・文の実行時間の上限・Sort/Hash Join/Hash
//! Aggregateの収集行数の上限は、追加の`--key value`引数(`<db-path>`の後、
//! 順不同)で指定する(第38章、`crate::database::ResourceLimits`・
//! `crate::server::ServerConfig`を参照)。`SET`文のような実行時のSQL構文では
//! なく起動引数にしたのは、これらがセッションではなくサーバープロセス全体の
//! 運用ポリシーだからである(`crate::database::ResourceLimits`のドキュメント
//! を参照)。
//!
//! ```text
//! cargo run -- --serve 127.0.0.1:5432 example.db \
//!     --workers 8 --queue 32 --statement-timeout-ms 5000 --max-rows 1000000
//! ```
//!
//! # Graceful Shutdown
//!
//! サーバーモードはCtrl-C(SIGINT)を`ctrlc`クレートで受け取り、
//! [`minidb::server::ShutdownHandle::trigger`]を呼ぶ。SIGINTのハンドラの中では
//! 呼んでよい処理がシグナルセーフな一部の関数に限られる(OSのシグナル配送の
//! 一般的な制約であり、Rust固有ではない)ため、この教材は生の`libc`シグナル
//! ハンドラを自作せず、`ctrlc`クレートに任せる。`ctrlc`はハンドラ内で
//! 安全な操作だけを行い、実際のシャットダウン処理(`Server::run`のループが
//! フラグを見て抜ける)はシグナルハンドラの外、通常のスレッドの上で進む。

use std::env;
use std::io::{self, BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

use minidb::database::ResourceLimits;
use minidb::server::ServerConfig;
use minidb::{Database, Server, SharedDatabase, Session};

fn main() {
    let mut args = env::args();
    let _program_name = args.next();

    match args.next() {
        Some(flag) if flag == "--serve" => run_server(args),
        first_arg => run_repl(first_arg),
    }
}

/// `--serve <addr> [db-path] [--workers N] [--queue N] [--statement-timeout-ms N] [--max-rows N]`を処理する。
fn run_server(mut args: env::Args) {
    let Some(addr) = args.next() else {
        eprintln!("使い方: minidb --serve <addr> [db-path] [--workers N] [--queue N] [--statement-timeout-ms N] [--max-rows N]");
        std::process::exit(1);
    };

    let remaining: Vec<String> = args.collect();
    let (db_path, options) = match remaining.first() {
        Some(first) if !first.starts_with("--") => (Some(first.clone()), &remaining[1..]),
        _ => (None, &remaining[..]),
    };
    let mut config = ServerConfig::default();
    let mut limits = ResourceLimits::default();
    if let Err(message) = parse_server_options(options, &mut config, &mut limits) {
        eprintln!("エラー: {message}");
        std::process::exit(1);
    }

    let db = match db_path {
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
    shared.set_resource_limits(limits);
    let server = match Server::bind_with_config(&addr, shared, config) {
        Ok(server) => server,
        Err(e) => {
            eprintln!("エラー: {addr}へbindできませんでした: {e}");
            return;
        }
    };

    let shutdown = server.shutdown_handle();
    if let Err(e) = ctrlc::set_handler(move || shutdown.trigger()) {
        eprintln!("警告: Ctrl-Cハンドラを登録できませんでした({e})。Graceful Shutdownは内部APIからのみ利用できます。");
    }

    println!("minidb: {addr}で接続を待機しています(ワーカー{}本、キュー{}件)", config.workers, config.queue_capacity);
    if let Err(e) = server.run() {
        eprintln!("エラー: サーバーが終了しました: {e}");
    }
}

/// `--workers`・`--queue`・`--statement-timeout-ms`・`--max-rows`を`config`・
/// `limits`へ反映する。順不同の`--key value`ペアの並びとして解釈する。
fn parse_server_options(options: &[String], config: &mut ServerConfig, limits: &mut ResourceLimits) -> Result<(), String> {
    let mut iter = options.iter();
    while let Some(key) = iter.next() {
        let value = iter.next().ok_or_else(|| format!("{key}に値がありません"))?;
        match key.as_str() {
            "--workers" => config.workers = parse_positive(key, value)?,
            "--queue" => config.queue_capacity = parse_nonnegative(key, value)?,
            "--statement-timeout-ms" => limits.statement_timeout = Some(Duration::from_millis(parse_nonnegative(key, value)? as u64)),
            "--max-rows" => limits.max_operator_rows = Some(parse_positive(key, value)?),
            other => return Err(format!("未知のオプションです: {other}")),
        }
    }
    Ok(())
}

fn parse_positive(key: &str, value: &str) -> Result<usize, String> {
    let parsed: usize = value.parse().map_err(|_| format!("{key}の値が数値ではありません: {value}"))?;
    if parsed == 0 {
        return Err(format!("{key}には1以上の数値を指定してください"));
    }
    Ok(parsed)
}

fn parse_nonnegative(key: &str, value: &str) -> Result<usize, String> {
    value.parse().map_err(|_| format!("{key}の値が数値ではありません: {value}"))
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
