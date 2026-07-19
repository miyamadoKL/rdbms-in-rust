//! [`crate::statistics::TableStats`]から選択率・行数を見積もる推定式(第27章)。
//!
//! この章の推定式は、`EXPLAIN`/`EXPLAIN ANALYZE`が表示する参考値としてのみ
//! 使う。`physical_plan::optimize`のアクセスパス・Join方式の選択ロジックは
//! この章では変更しない(選択にコストを使い始めるのは第28章のCost Model)。
//!
//! # 統計が無い場合のデフォルト選択率
//!
//! `ANALYZE`を実行していない列に対する述語は、PostgreSQLの`selfuncs.c`が
//! 使う慣用定数にフォールバックする。
//!
//! * 等値述語(`col = 定数`): [`DEFAULT_EQ_SEL`] = 0.005
//! * 不等号述語(`col > / >= / < / <=`): [`DEFAULT_INEQ_SEL`] = 1/3
//!
//! どちらも「典型的なテーブルでは、値の分布についてまったく手がかりが
//! 無くても、このくらいの割合の行が選ばれるだろう」という経験則であり、
//! PostgreSQLのソースコード上のコメントでも同じ値・同じ理由で使われている
//! (本文で出典を示す)。

use std::cmp::Ordering;

use crate::statistics::ColumnStats;
use crate::types::{Value, compare_values};

/// 等値述語のデフォルト選択率(統計が無い場合)。出典は本文を参照。
pub const DEFAULT_EQ_SEL: f64 = 0.005;

/// 不等号述語のデフォルト選択率(統計が無い場合)。出典は本文を参照。
pub const DEFAULT_INEQ_SEL: f64 = 1.0 / 3.0;

/// 統計が無いテーブルの行数のプレースホルダ。`ANALYZE`前は実際の行数を
/// 知りようが無いため、「小さすぎず大きすぎない」典型的な値として置く。
/// `ANALYZE`を実行すると、この値は[`crate::statistics::TableStats::row_count`]
/// (実測値)に置き換わる。
pub const DEFAULT_ROW_COUNT_ESTIMATE: u64 = 1000;

/// 範囲述語の向き。`col <op> value`という形の`<op>`にあたる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeOp {
    Gt,
    Ge,
    Lt,
    Le,
}

/// 等値述語(`col = value`)の選択率を見積もる。
///
/// Histogramがあれば、`value`が収まるバケツの行数比率を、そのバケツに
/// 含まれるDistinct値数(Histogram全体のDistinct値数をバケツ数で均等割りした
/// 近似)で割って見積もる。Histogramが無ければ`1 / NDV`(NDVも無ければ
/// [`DEFAULT_EQ_SEL`])にフォールバックする。
pub fn estimate_equality_selectivity(stats: Option<&ColumnStats>, value: &Value) -> f64 {
    let Some(stats) = stats else { return DEFAULT_EQ_SEL };

    if !stats.histogram.is_empty() {
        let total_rows: u64 = stats.histogram.iter().map(|b| b.row_count).sum();
        if total_rows == 0 {
            return DEFAULT_EQ_SEL;
        }
        let bucket_count = stats.histogram.len() as f64;
        // Histogram全体のDistinct値数を、バケツ数で均等に割った近似値。
        // 実際のバケツごとのDistinct値数は記録していないため、「Distinct値は
        // バケツ間に均等に散らばっている」という単純化した仮定を置く。
        let ndv_per_bucket = (stats.distinct_count as f64 / bucket_count).max(1.0);
        for bucket in &stats.histogram {
            if compare_values(value, &bucket.lower) != Ordering::Less && compare_values(value, &bucket.upper) != Ordering::Greater
            {
                let bucket_fraction = bucket.row_count as f64 / total_rows as f64;
                return bucket_fraction / ndv_per_bucket;
            }
        }
        // どのバケツにも収まらない(観測範囲の外の値)。稀な値として扱う。
        return DEFAULT_EQ_SEL;
    }

    if stats.distinct_count > 0 {
        1.0 / stats.distinct_count as f64
    } else {
        DEFAULT_EQ_SEL
    }
}

/// バケツ`[lower, upper]`のうち、`x <op> value`を満たす行の割合を見積もる。
///
/// バケツの両端(`lower`・`upper`)がそれぞれ述語を満たすかどうかだけを見て、
/// 「両端とも満たす→バケツ全体が満たす(1.0)」「両端とも満たさない→
/// バケツ全体が満たさない(0.0)」「片方だけ満たす→バケツ内に境界がある
/// ので、バケツ内で値は一様に分布していると仮定して半分(0.5)」の3通りで
/// 判定する。この一様分布の仮定は、バケツの境界という粗い情報しか使わずに
/// 見積もる以上避けられない単純化だが、バケツを細かく(等頻度に)刻んで
/// あるほど、バケツ1個の中の誤差は小さく抑えられる。
fn bucket_overlap_fraction(op: RangeOp, value: &Value, lower: &Value, upper: &Value) -> f64 {
    let satisfies = |x: &Value| -> bool {
        let ord = compare_values(x, value);
        match op {
            RangeOp::Gt => ord == Ordering::Greater,
            RangeOp::Ge => ord != Ordering::Less,
            RangeOp::Lt => ord == Ordering::Less,
            RangeOp::Le => ord != Ordering::Greater,
        }
    };
    match (satisfies(lower), satisfies(upper)) {
        (true, true) => 1.0,
        (false, false) => 0.0,
        _ => 0.5,
    }
}

/// 範囲述語(`col > / >= / < / <= value`)の選択率を見積もる。
///
/// Histogramがあれば、バケツごとの[`bucket_overlap_fraction`]を行数で
/// 重み付けして合計する。無ければMin/Maxを1個のバケツとみなして同じ計算を
/// 行う(線形補間)。Min/Maxも無ければ[`DEFAULT_INEQ_SEL`]にフォールバックする。
pub fn estimate_range_selectivity(stats: Option<&ColumnStats>, op: RangeOp, value: &Value) -> f64 {
    let Some(stats) = stats else { return DEFAULT_INEQ_SEL };

    if stats.histogram.is_empty() {
        return match (&stats.min, &stats.max) {
            (Some(min), Some(max)) => bucket_overlap_fraction(op, value, min, max),
            _ => DEFAULT_INEQ_SEL,
        };
    }

    let total_rows: u64 = stats.histogram.iter().map(|b| b.row_count).sum();
    if total_rows == 0 {
        return DEFAULT_INEQ_SEL;
    }
    let matched: f64 =
        stats.histogram.iter().map(|b| b.row_count as f64 * bucket_overlap_fraction(op, value, &b.lower, &b.upper)).sum();
    (matched / total_rows as f64).clamp(0.0, 1.0)
}

/// `AND`(2つの述語の連言)の選択率。独立性を仮定した積で見積もる。
pub fn estimate_and_selectivity(a: f64, b: f64) -> f64 {
    (a * b).clamp(0.0, 1.0)
}

/// `OR`(2つの述語の選言)の選択率。包除原理(独立性を仮定した
/// `1 - (1-a)(1-b)`)で見積もる。
pub fn estimate_or_selectivity(a: f64, b: f64) -> f64 {
    (1.0 - (1.0 - a) * (1.0 - b)).clamp(0.0, 1.0)
}

/// `NOT`(否定)の選択率。
pub fn estimate_not_selectivity(a: f64) -> f64 {
    (1.0 - a).clamp(0.0, 1.0)
}

/// 等値結合(`left.k = right.k`)の結果行数を見積もる、標準的な式。
///
/// `|L| × |R| / max(NDV_l, NDV_r)`。結合列がどちらも一意キー(NDVが行数と
/// 等しい)であれば、この式は「外側の各行に対して内側がちょうど1行だけ
/// 一致する」という主キー・外部キー結合の典型的な状況に一致する。
/// NDVが分からない列は、`left_ndv`・`right_ndv`にそれぞれの行数
/// (「値はすべて一意」という最も楽観的な既定値)を渡すことを想定する。
pub fn estimate_join_row_count(left_rows: u64, right_rows: u64, left_ndv: u64, right_ndv: u64) -> u64 {
    let max_ndv = left_ndv.max(right_ndv).max(1);
    ((left_rows as u128 * right_rows as u128) / max_ndv as u128).min(u64::MAX as u128) as u64
}

/// `GROUP BY`後の行数を見積もる。
///
/// `group_ndvs`は`GROUP BY`に並ぶ各列のNDV(Distinct値数)。それぞれの列の
/// 値が独立に組み合わさるとみなし、NDVの積をグループ数の見積もりとする。
/// ただし、積が`input_rows`(集約前の行数)を超えることはありえない
/// (1グループは少なくとも1行を含むため、グループ数は入力行数を超えない)
/// ので、`input_rows`で頭打ちにする。`u64`の掛け算オーバーフローを避ける
/// ため、`u128`で計算してから`input_rows`と比較する。
pub fn estimate_aggregate_row_count(group_ndvs: &[u64], input_rows: u64) -> u64 {
    if group_ndvs.is_empty() {
        // GROUP BYが無い集約(集約関数だけのSELECT)は、常にちょうど1行になる。
        return 1;
    }
    let mut product: u128 = 1;
    for &ndv in group_ndvs {
        product = product.saturating_mul(u128::from(ndv.max(1)));
        if product >= u128::from(input_rows) {
            return input_rows;
        }
    }
    product.min(u128::from(input_rows)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::statistics::Bucket;

    fn stats_with_histogram(distinct_count: u64, buckets: Vec<(i64, i64, u64)>) -> ColumnStats {
        ColumnStats {
            null_count: 0,
            distinct_count,
            min: buckets.first().map(|b| Value::BigInt(b.0)),
            max: buckets.last().map(|b| Value::BigInt(b.1)),
            histogram: buckets
                .into_iter()
                .map(|(lower, upper, row_count)| Bucket { lower: Value::BigInt(lower), upper: Value::BigInt(upper), row_count })
                .collect(),
        }
    }

    #[test]
    fn equality_without_stats_uses_default_selectivity() {
        assert_eq!(estimate_equality_selectivity(None, &Value::BigInt(1)), DEFAULT_EQ_SEL);
    }

    #[test]
    fn equality_without_histogram_uses_inverse_of_ndv() {
        let stats = ColumnStats { null_count: 0, distinct_count: 4, min: None, max: None, histogram: Vec::new() };
        assert_eq!(estimate_equality_selectivity(Some(&stats), &Value::BigInt(1)), 0.25);
    }

    #[test]
    fn equality_with_histogram_uses_matching_bucket() {
        // 10バケツ、各100行、それぞれ10個のdistinct値(全体で100distinct)を仮定する。
        let buckets: Vec<(i64, i64, u64)> = (0..10).map(|i| (i * 10, i * 10 + 9, 100)).collect();
        let stats = stats_with_histogram(100, buckets);
        // 値55はバケツ[50,59]に入る。バケツの行割合=100/1000=0.1、
        // バケツ内distinct数の近似=100/10=10。選択率=0.1/10=0.01。
        let selectivity = estimate_equality_selectivity(Some(&stats), &Value::BigInt(55));
        assert!((selectivity - 0.01).abs() < 1e-9, "selectivity={selectivity}");
    }

    #[test]
    fn equality_outside_histogram_range_falls_back_to_default() {
        let stats = stats_with_histogram(10, vec![(0, 9, 10)]);
        assert_eq!(estimate_equality_selectivity(Some(&stats), &Value::BigInt(999)), DEFAULT_EQ_SEL);
    }

    #[test]
    fn range_without_stats_uses_default_selectivity() {
        assert_eq!(estimate_range_selectivity(None, RangeOp::Gt, &Value::BigInt(1)), DEFAULT_INEQ_SEL);
    }

    #[test]
    fn range_with_min_max_interpolates() {
        let stats = ColumnStats { null_count: 0, distinct_count: 0, min: Some(Value::BigInt(0)), max: Some(Value::BigInt(100)), histogram: Vec::new() };
        // 0〜100の範囲に対し value=100 での `>` は、上限自身なので満たさない
        // (バケツ上端も下端も value=100 を上回らないため overlap=0.0)。
        assert_eq!(estimate_range_selectivity(Some(&stats), RangeOp::Gt, &Value::BigInt(100)), 0.0);
        // value=-1 での `>` は範囲全体が満たすので1.0。
        assert_eq!(estimate_range_selectivity(Some(&stats), RangeOp::Gt, &Value::BigInt(-1)), 1.0);
    }

    #[test]
    fn range_with_histogram_sums_matching_buckets() {
        let buckets: Vec<(i64, i64, u64)> = vec![(0, 9, 10), (10, 19, 10), (20, 29, 10)];
        let stats = stats_with_histogram(30, buckets);
        // value=15での`>=`: バケツ[0,9]は不一致(0.0)、[10,19]は境界を含む(0.5)、
        // [20,29]は全て一致(1.0)。合計=(0+5+10)/30=0.5。
        let selectivity = estimate_range_selectivity(Some(&stats), RangeOp::Ge, &Value::BigInt(15));
        assert!((selectivity - 0.5).abs() < 1e-9, "selectivity={selectivity}");
    }

    #[test]
    fn and_multiplies_selectivities() {
        assert!((estimate_and_selectivity(0.5, 0.4) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn or_uses_inclusion_exclusion() {
        // 1 - (1-0.5)(1-0.5) = 0.75
        assert!((estimate_or_selectivity(0.5, 0.5) - 0.75).abs() < 1e-9);
    }

    #[test]
    fn not_complements_selectivity() {
        assert!((estimate_not_selectivity(0.3) - 0.7).abs() < 1e-9);
    }

    #[test]
    fn join_cardinality_uses_standard_formula() {
        // 主キー(NDV=1000)対、外部キー(NDV=1000)。1000行×1000行/1000=1000行。
        assert_eq!(estimate_join_row_count(1000, 1000, 1000, 1000), 1000);
    }

    #[test]
    fn join_cardinality_uses_max_ndv() {
        // 左1000行(NDV=10)、右100行(NDV=100)。1000*100/100=1000。
        assert_eq!(estimate_join_row_count(1000, 100, 10, 100), 1000);
    }

    #[test]
    fn aggregate_row_count_multiplies_group_ndvs() {
        assert_eq!(estimate_aggregate_row_count(&[10, 20], 10_000), 200);
    }

    #[test]
    fn aggregate_row_count_saturates_at_input_rows() {
        assert_eq!(estimate_aggregate_row_count(&[1000, 1000, 1000], 500), 500);
    }

    #[test]
    fn aggregate_row_count_without_group_by_is_one() {
        assert_eq!(estimate_aggregate_row_count(&[], 12345), 1);
    }
}
