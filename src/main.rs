//! minidbのREPL。
//!
//! 標準入力からSQLを1行ずつ読み、`Database::execute`に渡して結果を表示する。
//! `\q`を入力すると終了する。

use std::io::{self, BufRead, Write};

use minidb::Database;

fn main() {
    let mut db = Database::memory();
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
}

/// プロンプトを表示する。バッファリングされた標準出力を即座に流すため`flush`する。
fn prompt(stdout: &mut io::Stdout) {
    print!("minidb> ");
    stdout.flush().ok();
}
