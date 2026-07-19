//! 実行時間が閾値を超えた文を記録するSlow Query Log(第39章)。
//!
//! `crate::database::ResourceLimits::slow_query_threshold`(サーバー起動引数
//! `--slow-query-threshold-ms`)を超えた文を、SQL文・実行時間・行数の3点で
//! stderrへ1行出力する。ログファイルではなくstderrを選んだ理由は、ログ
//! ローテーションのようなファイル運用をこの教材の範囲に持ち込まず、
//! 「サーバーの標準エラー出力をどこへ流すか」を利用者(`systemd`のジャーナル、
//! シェルのリダイレクト等)へ委ねられるからである。
//!
//! # 呼び出し箇所は2つ、経路は独立
//!
//! [`maybe_log`]は2箇所から呼ぶ。
//!
//! * [`crate::database::Database::execute_bound_statement`](第9章以来の
//!   低レベルAPI、REPL・埋め込み用途・大半のテストが使う`Database::execute`の
//!   実体)。
//! * [`crate::session::Session::execute`](第37章、REPL・Serverが実際に
//!   使う`PREPARE`・`EXECUTE`に対応した経路)の、`BEGIN`・`COMMIT`・
//!   `ROLLBACK`・`PREPARE`・`EXECUTE`・`DEALLOCATE`のいずれでもない文。
//!
//! この2つは同じSQL文字列に対して同時に呼ばれることはない
//! (`Session`は`Database::execute_bound_statement`を経由しない独立した
//! 実行経路を持つ、`crate::session`モジュール冒頭を参照)ため、二重記録は
//! 起きない。`EXECUTE name`で実行される`PREPARE`済みの文は、`PREPARE`時の
//! SQL文ではなく`EXECUTE name`という呼び出し文字列自体では記録しない
//! (この章の限界節を参照)。

use std::io::Write;
use std::time::Duration;

use crate::database::QueryResult;
use crate::error::DbResult;

/// `threshold`(`None`なら記録しない)を`elapsed`が超えていれば、SQL文・
/// 実行時間・行数をstderrへ1行記録する。
pub(crate) fn maybe_log(threshold: Option<Duration>, sql: &str, elapsed: Duration, result: &DbResult<QueryResult>) {
    log_to(&mut std::io::stderr(), threshold, sql, elapsed, result);
}

/// [`maybe_log`]の本体。書き込み先を注入できる形にしてあるのは、テストが
/// 実際のstderrを奪い合わずに出力内容を確認できるようにするためである
/// (`#[test]`は既定で並行に走るため、実プロセスのstderrを差し替える方式は
/// テスト間で競合する)。
///
/// 行数は成功した結果だけが持つ(`QueryResult::rows`)。エラーで終わった文は
/// `rows=0`として記録する(コマンドタグの文字列を追加でパースするより単純
/// であり、Slow Query Logの目的――どの文がどれだけ時間を使ったか――には
/// 十分)。
fn log_to(out: &mut impl Write, threshold: Option<Duration>, sql: &str, elapsed: Duration, result: &DbResult<QueryResult>) {
    let Some(threshold) = threshold else { return };
    if elapsed < threshold {
        return;
    }
    let rows = result.as_ref().map(|r| r.rows().len()).unwrap_or(0);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    let _ = writeln!(
        out,
        "[slow query] {:.3}ms rows={rows} outcome={outcome} sql={}",
        elapsed.as_secs_f64() * 1000.0,
        sql.trim()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Column, DataType, Schema, Tuple, Value};

    fn sample_result(row_count: usize) -> DbResult<QueryResult> {
        let schema = Schema::new(vec![Column::new("n", DataType::BigInt, false)]);
        let rows = (0..row_count)
            .map(|i| Tuple::new(&schema, vec![Value::BigInt(i as i64)]).unwrap())
            .collect();
        Ok(QueryResult::for_test(schema, rows))
    }

    #[test]
    fn logs_when_elapsed_meets_or_exceeds_the_threshold() {
        let mut out = Vec::new();
        log_to(&mut out, Some(Duration::from_millis(100)), "SELECT * FROM t", Duration::from_millis(150), &sample_result(3));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("rows=3"), "{text}");
        assert!(text.contains("outcome=ok"), "{text}");
        assert!(text.contains("SELECT * FROM t"), "{text}");
    }

    #[test]
    fn does_not_log_when_elapsed_is_under_the_threshold() {
        let mut out = Vec::new();
        log_to(&mut out, Some(Duration::from_millis(100)), "SELECT * FROM t", Duration::from_millis(10), &sample_result(3));
        assert!(out.is_empty());
    }

    #[test]
    fn does_not_log_when_no_threshold_is_configured() {
        let mut out = Vec::new();
        log_to(&mut out, None, "SELECT * FROM t", Duration::from_secs(999), &sample_result(3));
        assert!(out.is_empty());
    }

    #[test]
    fn logs_a_failed_statement_as_zero_rows_with_error_outcome() {
        let mut out = Vec::new();
        let err: DbResult<QueryResult> = Err(crate::error::DbError::TableNotFound("t".to_string()));
        log_to(&mut out, Some(Duration::from_millis(1)), "SELECT * FROM t", Duration::from_millis(5), &err);
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("rows=0"), "{text}");
        assert!(text.contains("outcome=error"), "{text}");
    }
}
