//! minidbのREPL。
//!
//! 標準入力からSQLを1行ずつ読み、`Database::execute`に渡して結果を表示する。
//! `\q`を入力すると終了する。
//!
//! 起動引数にファイルパスを渡すと、そのパスを`Database::open`(第16章)で開き、
//! 永続モードで動く(`cargo run -- example.db`)。引数を渡さなければ、これまで
//! どおり`Database::memory`のインメモリモードで動く。永続モードで終了すると
//! きは、`\q`の入力でも標準入力のEOFでも、抜ける前に必ず`Database::flush`を
//! 呼び、キャッシュされた変更をファイルへ書き戻す。

use std::env;
use std::io::{self, BufRead, Write};

use minidb::Database;

fn main() {
    let mut args = env::args();
    let _program_name = args.next();
    let mut db = match args.next() {
        Some(path) => match Database::open(&path) {
            Ok(db) => db,
            Err(e) => {
                eprintln!("エラー: {path}を開けませんでした: {e}");
                return;
            }
        },
        None => Database::memory(),
    };

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

        match db.execute(input) {
            Ok(result) => println!("{result}"),
            Err(e) => println!("エラー: {e}"),
        }
        prompt(&mut stdout);
    }

    if let Err(e) = db.flush() {
        eprintln!("エラー: 終了時のflushに失敗しました: {e}");
    }
}

/// プロンプトを表示する。バッファリングされた標準出力を即座に流すため`flush`する。
fn prompt(stdout: &mut io::Stdout) {
    print!("minidb> ");
    stdout.flush().ok();
}
