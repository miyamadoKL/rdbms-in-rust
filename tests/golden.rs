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
use minidb::{Token, TokenKind, tokenize};

/// `sql`を文へ分ける。`;`の文字だけを見て`str::split`する素朴な実装だと、
/// 文字列リテラル(`'a;b'`)やコメント(`-- a;b`、`/* a;b */`)の中に現れる`;`まで
/// 文の区切りとして誤認してしまう。字句解析器(`tokenize`)を通して
/// `TokenKind::Semicolon`だけを区切りとして扱い、各`Token`が持つ`Span`
/// (元のソース上のバイト範囲)で元の文字列をスライスすることで、文字列
/// リテラルやコメントの内側の`;`を安全に無視する。
///
/// 返るスライスは元の`sql`の一部をそのまま(コメント・空白を含めて)切り出した
/// ものであり、字句解析結果から再構築したものではない。
///
/// `tokenize`は成功か失敗かのどちらかしか返さず(閉じない文字列リテラルなどに
/// 遭遇した時点でエラーを返し、そこまでに読めていたTokenも捨てる)、部分的な
/// Token列を取り出す手段が無い。そのため、字句解析器自体が失敗するSQL(閉じない
/// 文字列リテラルなど)を期待値にするgoldenケースは、ファイル全体を対象に
/// `tokenize`する限り「文へ分ける」段階そのものがpanicしてしまう。この関数は
/// `tokenize`が失敗した場合、ファイル全体を分割せず1個の文としてそのまま返す
/// フォールバックを取る。字句解析エラーは(分割を経ずに)後段の`db.execute`が
/// 検出し、期待どおり`ERROR: `付きの結果になる。このフォールバックが働く
/// goldenケースは、壊れた文とその前後を分けて実行する必要が無いよう、
/// 1ファイル1文(壊れた文だけ)にとどめる前提とする。
fn split_statements(sql: &str) -> Vec<&str> {
    match tokenize(sql) {
        Ok(tokens) => split_by_tokens(sql, &tokens),
        Err(_) => {
            let text = sql.trim();
            if text.is_empty() { Vec::new() } else { vec![text] }
        }
    }
}

/// `tokenize`に成功した`sql`を、`TokenKind::Semicolon`ごとに文へ分ける。
///
/// 区切りの判定は、切り出した文字列を`trim`して空かどうかではなく、区切りの
/// 間に`Semicolon`・`Eof`以外のTokenが1個でも現れたかどうかで行う。コメントは
/// 字句解析の段階でTokenを1個も生成しないため、`SELECT 1; -- trailing`の
/// ように最後の`;`の後ろにコメントしか無い場合、そこには実Tokenが無いので
/// 文として扱わない。`text.trim().is_empty()`による判定だと、コメントの
/// 文字自体は空白ではないため、コメントだけの断片を空文として扱ってしまい、
/// それを`db.execute`に渡してしまう(空文の実行は別のエラーになる、あるいは
/// 意図しない挙動になる)。
fn split_by_tokens<'a>(sql: &'a str, tokens: &[Token]) -> Vec<&'a str> {
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut has_real_token_since_start = false;
    for token in tokens {
        match token.kind {
            TokenKind::Semicolon => {
                if has_real_token_since_start {
                    statements.push(sql[start..token.span.start].trim());
                }
                start = token.span.end;
                has_real_token_since_start = false;
            }
            TokenKind::Eof => {
                if has_real_token_since_start {
                    statements.push(sql[start..token.span.start].trim());
                }
            }
            _ => has_real_token_since_start = true,
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
    fn ignores_a_trailing_line_comment_after_the_last_semicolon() {
        // コメントは字句解析でTokenを生成しないため、最後の`;`の後ろに
        // コメントしか無い場合は文として扱わない。`text.trim().is_empty()`
        // による判定だと、コメントの文字自体は空白ではないので誤って
        // 2文目として拾ってしまう(このテストが守る回帰)。
        assert_eq!(
            split_statements("SELECT 1; -- trailing comment only"),
            vec!["SELECT 1"]
        );
    }

    #[test]
    fn ignores_a_trailing_block_comment_after_the_last_semicolon() {
        assert_eq!(
            split_statements("SELECT 1; /* trailing block comment only */"),
            vec!["SELECT 1"]
        );
    }

    #[test]
    fn falls_back_to_a_single_statement_when_the_whole_file_fails_to_tokenize() {
        // `tokenize`は成功か失敗かのどちらかしか返さないため、閉じない文字列
        // リテラルのようなSQL全体の字句解析エラーがあると、Span基準の分割が
        // そもそもできない。この場合はファイル全体を1文として返し、実際の
        // 字句エラーは`db.execute`側で検出させる(モジュールのドキュメント
        // コメント参照)。
        assert_eq!(
            split_statements("SELECT 'unterminated"),
            vec!["SELECT 'unterminated"]
        );
    }

    #[test]
    fn blank_input_has_no_statements() {
        // 空文字列は`tokenize`自体には成功する(`Eof`だけのToken列になる)ため
        // フォールバック経路ではなく通常の分割経路を通るが、実Tokenが1個も
        // 無いので文は0個になる。
        assert_eq!(split_statements(""), Vec::<&str>::new());
    }


    #[test]
    fn does_not_split_on_semicolon_inside_block_comment() {
        assert_eq!(
            split_statements("SELECT 1; /* a;b */ SELECT 2;"),
            vec!["SELECT 1", "/* a;b */ SELECT 2"]
        );
    }
}
