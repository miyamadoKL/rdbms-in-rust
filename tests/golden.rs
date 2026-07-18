//! SQL Golden Test ランナーの骨格。
//!
//! `tests/golden/*.sql` と同名の `*.expected` をペアにして突き合わせる。
//! まだクエリエンジンが存在しないため、`run_sql` は「入力をそのまま返す」
//! 仮実装(エコー)になっている。第5章以降でここを実際のエンジン呼び出しへ
//! 差し替えていく。

use std::fs;
use std::path::{Path, PathBuf};

/// 仮実装: クエリエンジンがまだ無いので、SQLをそのままエコーするだけ。
///
/// 第5章以降、ここを `minidb` のパーサ・実行エンジン呼び出しに置き換える。
fn run_sql(sql: &str) -> String {
    sql.to_string()
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
            actual, expected,
            "golden test 不一致: {:?}",
            sql_path.file_name().unwrap()
        );
    }
}
