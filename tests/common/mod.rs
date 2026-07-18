//! 統合テスト共通のヘルパー。
//!
//! `tests/`配下の各ファイルは別々のクレートとしてコンパイルされるため、
//! 一時DBを組み立てる処理をここに集約し、`mod common;`で読み込んで使う。

use minidb::{Database, DbResult, QueryResult};

/// インメモリのDatabaseを1つ作る。各テストはこの関数から独立したDBを得る。
#[allow(dead_code)]
pub fn temp_db() -> Database {
    Database::memory()
}

/// インメモリDBを1つ作り、SQLを1文実行した結果を返す。
///
/// 1本のSQLを実行して結果だけを確認したいテストのための近道。
#[allow(dead_code)]
pub fn execute_sql(sql: &str) -> DbResult<QueryResult> {
    temp_db().execute(sql)
}
