//! SQL Golden Test ランナー。
//!
//! `tests/golden/*.sql` と同名の `*.expected` をペアにして突き合わせる。
//! `.sql`ファイルは`;`区切りで複数の文を持てる。すべての文は同じ`Database`を
//! 使い回して順に実行し、各文の結果(成功時は`QueryResult`の表示形式、失敗時は
//! `ERROR: `に続けてエラーメッセージ)を空行区切りで連結したものが期待値になる。
//! `CREATE TABLE`と`INSERT`を1ファイルにまとめて書けるのはこのためであり、
//! 第9章の時点では1ファイル1文しか置けなかった制約をこの章で外している。

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::temp_db;

/// `sql`を`;`区切りの文へ分け、1つの`Database`で順に実行する。
/// 各文の結果を空行区切りで連結した文字列を返す。
fn run_sql(sql: &str) -> String {
    let mut db = temp_db();
    sql.split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(|statement| match db.execute(statement) {
            Ok(result) => result.to_string(),
            Err(e) => format!("ERROR: {e}"),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `tests/golden/` 以下の `.sql` ファイルを列挙する。
fn collect_sql_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("golden test dir {:?} を開けません: {e}", dir))
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_else(|e| panic!("golden test dir {:?} の列挙中にエラー: {e}", dir))
        .into_iter()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("sql"))
        .collect();
    files.sort();
    files
}

#[test]
fn golden_tests_pass() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let sql_files = collect_sql_files(&dir);
    assert!(
        !sql_files.is_empty(),
        "tests/golden/ にサンプルの .sql が見つかりません"
    );

    for sql_path in sql_files {
        let expected_path = sql_path.with_extension("expected");
        assert!(
            expected_path.exists(),
            "{:?} に対応する .expected がありません",
            sql_path
        );

        let sql = fs::read_to_string(&sql_path)
            .unwrap_or_else(|e| panic!("{:?} を読めません: {e}", sql_path));
        let expected = fs::read_to_string(&expected_path)
            .unwrap_or_else(|e| panic!("{:?} を読めません: {e}", expected_path));

        let actual = run_sql(&sql);
        assert_eq!(
            actual,
            expected,
            "golden test 不一致: {:?}",
            sql_path.file_name().unwrap()
        );
    }
}
