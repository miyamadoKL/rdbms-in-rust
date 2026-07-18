//! SQL Golden Test ランナー。
//!
//! `tests/golden/*.sql` と同名の `*.expected` をペアにして突き合わせる。
//! `run_sql` は `Database::execute` を呼び出し、成功時は `QueryResult` の表示形式を、
//! 失敗時は `ERROR: `に続けてエラーメッセージを返す。

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::execute_sql;

/// SQLを1本実行し、Golden Testと突き合わせるための文字列表現を返す。
fn run_sql(sql: &str) -> String {
    match execute_sql(sql) {
        Ok(result) => result.to_string(),
        Err(e) => format!("ERROR: {e}"),
    }
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
