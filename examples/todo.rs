//! Embedded APIを一通り使うサンプルアプリケーション(第40章)。
//!
//! ToDoリストを題材に、この教材が積み上げてきた機能を1つのプログラムから
//! 使ってみる。`cargo run --example todo`で実行できる。
//!
//! - `CREATE TABLE`(第9章)、`INSERT`・`SELECT`・`UPDATE`・`DELETE`(第10章)
//! - `JOIN`(第22章)、`GROUP BY`による集約(第21章)
//! - `BEGIN`/`COMMIT`/`ROLLBACK`によるトランザクション(第30章)
//! - `PREPARE`/`EXECUTE`(第37章)
//!
//! `PREPARE`・`EXECUTE`・`DEALLOCATE`は`Database::execute`では受け付けず
//! `Session`を経由する必要がある(`src/session.rs`モジュール冒頭を参照)ため、
//! このサンプルは最初から`SharedDatabase`と`Session`を組み立てて使う。
//! サーバー(第36章)を経由しない、単一プロセス内のEmbedded APIとしての
//! 使い方である。

use std::sync::Arc;

use minidb::{Database, SharedDatabase, Session};

/// SQLを1文実行し、結果をラベル付きで表示する。`unwrap`しているのは、この
/// サンプルが「正常系が最後まで通ること」を目的としているためで、エラー処理の
/// 作法自体は他の章の題材である。
fn run(session: &mut Session, label: &str, sql: &str) {
    println!("-- {label}");
    println!("{sql}");
    match session.execute(sql) {
        Ok(result) => println!("{result}\n"),
        Err(err) => panic!("{label}が失敗した: {err}"),
    }
}

fn main() {
    let shared = Arc::new(SharedDatabase::new(Database::memory()));
    let mut session = Session::new(Arc::clone(&shared));

    // --- CREATE: リストとToDoの2テーブル ---
    run(
        &mut session,
        "リストのテーブルを作る",
        "CREATE TABLE lists (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
    );
    run(
        &mut session,
        "ToDoのテーブルを作る(listsへの参照はBIGINTの列で表す、外部キー制約は発展編Bの対象)",
        "CREATE TABLE todos (id BIGINT PRIMARY KEY, list_id BIGINT NOT NULL, title TEXT NOT NULL, done BOOLEAN NOT NULL)",
    );

    // --- INSERT ---
    run(&mut session, "リストを2つ登録する", "INSERT INTO lists VALUES (1, 'Work'), (2, 'Home')");

    // --- PREPARE/EXECUTE: 同じ形のINSERTを繰り返す ---
    run(
        &mut session,
        "ToDoを追加するPrepared Statementを用意する",
        "PREPARE add_todo AS INSERT INTO todos VALUES ($1, $2, $3, FALSE)",
    );
    run(&mut session, "Work宛てのToDoを2件追加する", "EXECUTE add_todo(1, 1, 'Reply to the review')");
    run(&mut session, "", "EXECUTE add_todo(2, 1, 'Write the release notes')");
    run(&mut session, "Home宛てのToDoを1件追加する", "EXECUTE add_todo(3, 2, 'Buy milk')");

    // --- UPDATE ---
    run(&mut session, "1件を完了にする", "UPDATE todos SET done = TRUE WHERE id = 1");

    // --- JOIN: ToDoとリスト名を結合して見る ---
    run(
        &mut session,
        "ToDoをリスト名つきで一覧する(JOIN)",
        "SELECT lists.name, todos.title, todos.done FROM todos JOIN lists ON todos.list_id = lists.id ORDER BY todos.id",
    );

    // --- 集約: リストごとの件数 ---
    run(
        &mut session,
        "リストごとのToDo件数を集計する(GROUP BY)",
        "SELECT list_id, COUNT(*) FROM todos GROUP BY list_id ORDER BY list_id",
    );

    // --- トランザクション: 複数の変更を1つの単位にまとめる ---
    run(
        &mut session,
        "Workリストの残りを一括で完了にする(BEGINで開始)",
        "BEGIN",
    );
    run(&mut session, "", "UPDATE todos SET done = TRUE WHERE list_id = 1 AND done = FALSE");
    run(&mut session, "ここまでの変更を確定する", "COMMIT");

    // ロールバックも実際に踏んでおく。誤って全件消してしまいそうになった
    // 操作を、コミットする前に取り消せることを確認する。
    run(&mut session, "誤って全ToDoを消しそうになる(まだ確定していない)", "BEGIN");
    run(&mut session, "", "DELETE FROM todos");
    run(
        &mut session,
        "COUNT(*)は0になっている(このトランザクションの中からは見える)",
        "SELECT COUNT(*) FROM todos",
    );
    run(&mut session, "やはり取り消す", "ROLLBACK");
    run(&mut session, "ROLLBACK後、ToDoは元通り残っている", "SELECT COUNT(*) FROM todos");

    // --- 後始末 ---
    run(&mut session, "Prepared Statementを手放す", "DEALLOCATE add_todo");

    println!("すべての操作が完了した。");
}
