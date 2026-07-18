//! 永続モードの`Database`(`Database::open`、第16章)が、SQL経由で作った
//! テーブルとその行をプロセスの再起動をまたいで保持することを確認する
//! 統合テスト。
//!
//! `create`→`insert`→(`Database`をdropしてプロセスの再起動を模す)→`open`→
//! `select`という、この章の到達点そのものを確認するテストが中心になる。
//! これに加えて、同じSQLをインメモリモード(`Database::memory`)と永続モード
//! (`Database::open`)の両方に流し、結果が一致することも確認する。

mod common;

use common::temp_db_path;
use minidb::{Database, Value};

#[test]
fn create_insert_restart_open_select_preserves_the_table_and_its_rows() {
    let path = temp_db_path("restart-basic");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        db.flush().unwrap();
        // `db`はここでスコープを抜けてdropされる。プロセスの再起動を模している。
    }

    let mut db = Database::open(&path).unwrap();
    let result = db.execute("SELECT id, name FROM users WHERE id = 2").unwrap();
    assert_eq!(result.rows().len(), 1);
    assert_eq!(
        result.rows()[0].values(),
        &[Value::BigInt(2), Value::Text("Bob".to_string())]
    );

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn update_and_delete_over_sql_persist_across_reopen() {
    // UPDATE/DELETEはRecordIdベースの`executor::storage_update`・
    // `storage_delete`(第13章のHeap Fileの仕組みの上)を通る。再起動後に
    // 書き換え・削除の結果がそのまま残っていることを確認する。
    let path = temp_db_path("restart-update-delete");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')")
            .unwrap();
        db.execute("UPDATE users SET name = 'Alicia' WHERE id = 1")
            .unwrap();
        db.execute("DELETE FROM users WHERE id = 3").unwrap();
        db.flush().unwrap();
    }

    let mut db = Database::open(&path).unwrap();
    let alice = db.execute("SELECT name FROM users WHERE id = 1").unwrap();
    assert_eq!(alice.rows()[0].values(), &[Value::Text("Alicia".to_string())]);

    let carol_gone = db.execute("SELECT id FROM users WHERE id = 3").unwrap();
    assert!(carol_gone.rows().is_empty());

    let remaining = db.execute("SELECT id FROM users").unwrap();
    assert_eq!(remaining.rows().len(), 2);

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn drop_table_over_sql_persists_across_reopen_and_the_name_can_be_reused() {
    let path = temp_db_path("restart-drop-table");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL)").unwrap();
        db.execute("DROP TABLE users").unwrap();
        db.flush().unwrap();
    }

    let mut db = Database::open(&path).unwrap();
    let result = db.execute("SELECT * FROM users");
    assert!(result.is_err());

    // 削除したテーブル名は再利用できる(第9章の不変条件)。再オープン後でも
    // 同じ名前で作り直せることを確認する。
    db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
        .unwrap();
    assert!(db.execute("SELECT * FROM users").unwrap().rows().is_empty());

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn opening_a_missing_path_creates_a_new_database() {
    let path = temp_db_path("fresh-open");
    assert!(!path.exists());

    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (id BIGINT NOT NULL)").unwrap();
    assert_eq!(
        db.execute("INSERT INTO t VALUES (1)").unwrap().to_string(),
        "INSERT 1"
    );

    std::fs::remove_file(&path).unwrap();
}

#[test]
fn memory_and_disk_backends_agree_on_the_same_sql() {
    // 第1部由来のインメモリ経路(executor::insert/update/delete)と、この章で
    // 追加した永続経路(executor::storage_insert/storage_update/storage_delete)
    // が、同じSQLに対して同じ結果を返すことを確認する。
    let setup = [
        "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
        "INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob'), (3, 'Carol')",
        "UPDATE users SET name = 'Bobby' WHERE id = 2",
        "DELETE FROM users WHERE id = 3",
    ];
    let query = "SELECT id, name FROM users WHERE id < 3";

    let mut memory_db = Database::memory();
    for statement in setup {
        memory_db.execute(statement).unwrap();
    }
    let memory_result = memory_db.execute(query).unwrap();

    let path = temp_db_path("mem-disk-parity");
    let mut disk_db = Database::open(&path).unwrap();
    for statement in setup {
        disk_db.execute(statement).unwrap();
    }
    let disk_result = disk_db.execute(query).unwrap();

    let memory_rows: Vec<_> = memory_result.rows().iter().map(|t| t.values()).collect();
    let disk_rows: Vec<_> = disk_result.rows().iter().map(|t| t.values()).collect();
    assert_eq!(memory_rows, disk_rows);

    std::fs::remove_file(&path).unwrap();
}
