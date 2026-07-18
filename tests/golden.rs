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
use minidb::{TokenKind, tokenize};

/// `sql`を文へ分ける。`;`の文字だけを見て`str::split`する素朴な実装だと、
/// 文字列リテラル(`'a;b'`)やコメント(`-- a;b`、`/* a;b */`)の中に現れる`;`まで
/// 文の区切りとして誤認してしまう。字句解析器(`tokenize`)を通して
/// `TokenKind::Semicolon`だけを区切りとして扱い、各`Token`が持つ`Span`
/// (元のソース上のバイト範囲)で元の文字列をスライスすることで、文字列
/// リテラルやコメントの内側の`;`を安全に無視する。
///
/// 返るスライスは元の`sql`の一部をそのまま(コメント・空白を含めて)切り出した
/// ものであり、字句解析結果から再構築したものではない。
fn split_statements(sql: &str) -> Vec<&str> {
    let tokens =
        tokenize(sql).unwrap_or_else(|e| panic!("golden testのSQLをtokenizeできません: {e}"));

    let mut statements = Vec::new();
    let mut start = 0usize;
    for token in &tokens {
        match token.kind {
            TokenKind::Semicolon => {
                let text = sql[start..token.span.start].trim();
                if !text.is_empty() {
                    statements.push(text);
                }
                start = token.span.end;
            }
            TokenKind::Eof => {
                let text = sql[start..token.span.start].trim();
                if !text.is_empty() {
                    statements.push(text);
                }
            }
            _ => {}
        }
    }
    statements
}

/// `sql`を文へ分け、1つの`Database`で順に実行する。
/// 各文の結果を空行区切りで連結した文字列を返す。
fn run_sql(sql: &str) -> String {
    let mut db = temp_db();
    split_statements(sql)
        .into_iter()
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

#[cfg(test)]
mod split_statements_tests {
    use super::split_statements;

    #[test]
    fn splits_multiple_statements_on_semicolon() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2;"),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn keeps_trailing_content_without_semicolon() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2"),
            vec!["SELECT 1", "SELECT 2"]
        );
    }

    #[test]
    fn ignores_trailing_whitespace_after_last_semicolon() {
        assert_eq!(split_statements("SELECT 1;   \n\t"), vec!["SELECT 1"]);
    }

    #[test]
    fn does_not_split_on_semicolon_inside_string_literal() {
        // 素朴な`split(';')`なら`SELECT 'a`と`b'`の2文に壊れてしまう。
        assert_eq!(split_statements("SELECT 'a;b';"), vec!["SELECT 'a;b'"]);
    }

    #[test]
    fn does_not_split_on_semicolon_inside_line_comment() {
        assert_eq!(
            split_statements("SELECT 1; -- a;b\nSELECT 2;"),
            vec!["SELECT 1", "-- a;b\nSELECT 2"]
        );
    }

    #[test]
    fn does_not_split_on_semicolon_inside_block_comment() {
        assert_eq!(
            split_statements("SELECT 1; /* a;b */ SELECT 2;"),
            vec!["SELECT 1", "/* a;b */ SELECT 2"]
        );
    }
}
