//! テーブル・列ごとの統計情報の型定義と、それを1回の全件走査から集計する
//! [`StatsCollector`](第27章)。
//!
//! `ANALYZE`(第27章)が集めるのは次の4種類である。
//!
//! * テーブルの行数(`TableStats::row_count`)。
//! * 列ごとのNULL数(`ColumnStats::null_count`)。
//! * 列ごとのDistinct値数(`ColumnStats::distinct_count`)。`HashSet`で
//!   **正確に**数える。HyperLogLog等の近似アルゴリズムは、この章では
//!   採らない。理由は本文([`book/src/ch27-statistics.md`])で説明するが、
//!   要点は2つある。第一に、この章の目的はCardinality Estimationの推定式
//!   そのものを検証することであり、「正確なDistinct値数を知っている前提で
//!   推定式が正しく機能するか」をまず確かめる段階では、近似誤差という
//!   もう1つの不確実性を持ち込みたくない。第二に、`HashSet`による厳密な
//!   カウントは実装がこの1関数に閉じる単純さを持つが、HyperLogLogは
//!   ハッシュ関数・レジスタ配列・調和平均による推定式など、この章の分量に
//!   見合わない複雑さを要求する。
//! * 列ごとの**MCV**(Most Common Values、最頻値。`ColumnStats::mcv`)と、
//!   MCVを除いた残余の値で組み立てる**Histogram**(`ColumnStats::histogram`)。
//!   Histogramは固定バケツ数([`HISTOGRAM_BUCKET_COUNT`])の**等頻度
//!   (equi-depth)**方式を採る(各バケツにほぼ同数の行が収まるよう境界を
//!   決める)。等幅(equi-width、値の範囲を等分する)ではなくこちらを選んだ
//!   理由、およびMCVとHistogramを分離した理由は、下記「MCVと等頻度Histogramの
//!   役割分担」を参照。PostgreSQLの`ANALYZE`が作る`pg_stats.histogram_bounds`・
//!   `pg_stats.most_common_vals`も、同じ役割分担(MCV+残余のequi-depth
//!   Histogram)を採っている。
//!
//! `Min`/`Max`/`Histogram`の対象は、[`crate::types::compare_values`]で
//! 比較できる列すべて(`BOOLEAN`・`BIGINT`・`TEXT`のいずれも)である。
//!
//! # MCVと等頻度Histogramの役割分担(第4部レビュー対応)
//!
//! 等頻度Histogramは「バケツに収まる行数を揃える」方式である。ある値が
//! 突出して多い(たとえば1000行中900行が同じ値)場合、その値1個だけで
//! 複数のバケツの目標行数を上回ってしまい、`build_equi_depth_histogram`は
//! 同じ値をまたいで複数のバケツに分割せざるを得ない。すると、その値に対する
//! `col = value`の等値述語は、値の一部しか含まない**先頭の1バケツだけ**を
//! 見て見積もることになり、実際の行数(900行)よりはるかに小さい値を返して
//! しまう。
//!
//! この問題を避けるため、事前に「1バケツの平均行数を超える頻度を持つ値」を
//! [`ColumnStats::mcv`]として個別に(値そのものと実際の頻度の組で)抜き出し、
//! Histogramはそれらを除いた**残余**の値だけで組み立てる。等値述語の推定
//! (`crate::estimator::estimate_equality_selectivity`)は、まずMCVに値が
//! 載っていればその実頻度を直接返し、載っていなければ残余のHistogramへ
//! フォールバックする。これにより、Histogramの各バケツは常に「均等に近い
//! 行数を持つ、複数のDistinct値の集まり」という前提を保ったまま、突出した
//! 値は個別に正確な頻度で扱える。
//!
//! MCVへ採用する条件は、「その値の出現回数が、実際にHistogramが作る
//! バケツ数(`min(非NULL行数, `[`HISTOGRAM_BUCKET_COUNT`]`)`、値の総数が
//! バケツ数を下回れば、それだけしかバケツはできない)で非NULL行数を割った
//! 平均バケツ行数を上回ること」である(「1バケツに収まるはずの行数より
//! 多い」=「1バケツ相当に押し込めるとHistogramの精度を損なう」値、という
//! 基準)。該当する値のうち、出現回数の多い順に最大[`MCV_MAX_ENTRIES`]件までを
//! 採用する。すべての値の出現回数がこの閾値以下(典型的には一意に近い列)で
//! あれば、`mcv`は空になり、Histogramはこれまでどおり全ての非NULL値から
//! 組み立てる。
//!
//! この閾値の定義上、平均を上回る値どうしの出現回数の合計は非NULL行数を
//! 超えられないため、[`MCV_MAX_ENTRIES`]-1件を超える値が同時にこの閾値を
//! 上回ることは(equi-depthのバケツ数が10である限り)実際には起こらない。
//! [`MCV_MAX_ENTRIES`]による打ち切りは、`Storage::set_table_stats`
//! (`crate::storage`)が外部から受け取る`ColumnStats`(この`extract_mcv`を
//! 経由しない)に対する防御的な上限として存在する。

use std::collections::HashSet;

use crate::types::{Schema, Tuple, Value, compare_values};

/// Histogramのバケツ数(固定)。
pub const HISTOGRAM_BUCKET_COUNT: usize = 10;

/// MCV(最頻値)として個別に保持する値の上限件数。
pub const MCV_MAX_ENTRIES: usize = 10;

/// Histogramの1バケツ。`[lower, upper]`(両端を含む)の範囲に、`row_count`件の
/// (MCVを除いた残余の)非NULL値が収まっている。
#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    pub lower: Value,
    pub upper: Value,
    pub row_count: u64,
}

/// 列1個の統計値。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ColumnStats {
    /// この列が`NULL`だった行数。
    pub null_count: u64,
    /// この列のDistinct値数(`NULL`を除く、正確な値。MCVに載る値も含む)。
    pub distinct_count: u64,
    /// この列の最小値(`NULL`を除く)。値が1件も無ければ`None`。
    pub min: Option<Value>,
    /// この列の最大値(`NULL`を除く)。
    pub max: Option<Value>,
    /// 最頻値のリスト(値, 出現回数)。出現回数の降順。最大
    /// [`MCV_MAX_ENTRIES`]件。採用条件はモジュール冒頭の説明を参照。
    pub mcv: Vec<(Value, u64)>,
    /// 等頻度Histogram。`mcv`に載った値を除いた残余の非NULL値から組み立てる
    /// (モジュール冒頭の説明を参照)。残余が1件も無ければ空の`Vec`。
    pub histogram: Vec<Bucket>,
}

/// テーブル1個の統計値。`columns`は`Schema::columns()`と同じ並び順・同じ
/// 列数を持つ。
#[derive(Debug, Clone, PartialEq)]
pub struct TableStats {
    pub row_count: u64,
    pub columns: Vec<ColumnStats>,
}

/// 1列ぶんの集計途中の状態。`StatsCollector::add_row`が行ごとに更新し、
/// `StatsCollector::finish`がMCVとHistogramを組み立てて[`ColumnStats`]へ
/// 変換する。
#[derive(Debug, Default)]
struct ColumnAccumulator {
    null_count: u64,
    distinct: HashSet<Value>,
    min: Option<Value>,
    max: Option<Value>,
    /// 非NULLの値を出現順のまま集めておく(MCV・Histogram組み立て用)。
    /// `finish`の時点でソートしてから、出現回数を数えてMCVを取り分け、
    /// 残余を等頻度に分割する。
    values: Vec<Value>,
}

/// テーブルを1回走査しながら[`TableStats`]を組み立てる集計器。
///
/// `ANALYZE`(第27章)は、`SeqScan`相当の全件走査(`Database::build_query_executor`
/// が組み立てる`Executor`)から1行ずつ`Tuple`を受け取り、その都度
/// `add_row`を呼ぶ。最後に`finish`を呼ぶと、MCV・Histogramの境界を確定させた
/// `TableStats`が得られる。
pub struct StatsCollector {
    row_count: u64,
    columns: Vec<ColumnAccumulator>,
}

impl StatsCollector {
    /// `schema`の列数ぶんの空の集計器を作る。
    pub fn new(schema: &Schema) -> Self {
        StatsCollector {
            row_count: 0,
            columns: (0..schema.len()).map(|_| ColumnAccumulator::default()).collect(),
        }
    }

    /// 1行を集計に反映する。
    pub fn add_row(&mut self, tuple: &Tuple) {
        self.row_count += 1;
        for (accumulator, value) in self.columns.iter_mut().zip(tuple.values()) {
            if value.is_null() {
                accumulator.null_count += 1;
                continue;
            }
            accumulator.distinct.insert(value.clone());
            if accumulator.min.as_ref().is_none_or(|min| compare_values(value, min) == std::cmp::Ordering::Less) {
                accumulator.min = Some(value.clone());
            }
            if accumulator.max.as_ref().is_none_or(|max| compare_values(value, max) == std::cmp::Ordering::Greater) {
                accumulator.max = Some(value.clone());
            }
            accumulator.values.push(value.clone());
        }
    }

    /// 集計を締め、MCV・Histogramを組み立てた[`TableStats`]を返す。
    pub fn finish(self) -> TableStats {
        let columns = self
            .columns
            .into_iter()
            .map(|mut accumulator| {
                accumulator.values.sort_by(compare_values);
                let (mcv, residual) = extract_mcv(&accumulator.values);
                let histogram = build_equi_depth_histogram(&residual);
                ColumnStats {
                    null_count: accumulator.null_count,
                    distinct_count: accumulator.distinct.len() as u64,
                    min: accumulator.min,
                    max: accumulator.max,
                    mcv,
                    histogram,
                }
            })
            .collect();
        TableStats { row_count: self.row_count, columns }
    }
}

/// ソート済みの非NULL値`sorted_values`から、MCV(最頻値)を切り出す。
///
/// 戻り値は`(mcv, residual)`。`residual`は`mcv`に採用した値をすべて除いた
/// 残りの値で、ソート順を保ったまま返す(`build_equi_depth_histogram`が
/// そのまま使える)。採用条件・上限件数はモジュール冒頭の説明を参照。
fn extract_mcv(sorted_values: &[Value]) -> (Vec<(Value, u64)>, Vec<Value>) {
    if sorted_values.is_empty() {
        return (Vec::new(), Vec::new());
    }

    // ソート済みなので、同じ値は連続して並ぶ。連続run(同じ値の連なり)を
    // 数えるだけで、値ごとの出現回数が求まる。
    let mut counts: Vec<(Value, u64)> = Vec::new();
    for value in sorted_values {
        match counts.last_mut() {
            Some((last_value, count)) if last_value == value => *count += 1,
            _ => counts.push((value.clone(), 1)),
        }
    }

    // `build_equi_depth_histogram`が実際に作るバケツ数は
    // `min(値の総数, HISTOGRAM_BUCKET_COUNT)`である(値の総数がバケツ数
    // より少なければ、その分だけしかバケツができない)。閾値もこの実際の
    // バケツ数に対する平均行数で揃える。固定の`HISTOGRAM_BUCKET_COUNT`を
    // 分母にすると、値の総数がバケツ数を下回る小さな列で「1回しか出現
    // しない値」まで平均を上回ってしまい、MCVがほぼ全値を飲み込んでしまう。
    let effective_bucket_count = sorted_values.len().min(HISTOGRAM_BUCKET_COUNT) as f64;
    let average_bucket_size = sorted_values.len() as f64 / effective_bucket_count;
    let mut candidates: Vec<(Value, u64)> =
        counts.iter().filter(|(_, count)| *count as f64 > average_bucket_size).cloned().collect();
    // 出現回数の降順。同数なら値の昇順(`counts`はソート済みの値順)で決定的に揃える。
    candidates.sort_by(|(value_a, count_a), (value_b, count_b)| {
        count_b.cmp(count_a).then_with(|| compare_values(value_a, value_b))
    });
    candidates.truncate(MCV_MAX_ENTRIES);

    if candidates.is_empty() {
        return (Vec::new(), sorted_values.to_vec());
    }

    let mcv_values: HashSet<&Value> = candidates.iter().map(|(value, _)| value).collect();
    let residual: Vec<Value> = sorted_values.iter().filter(|value| !mcv_values.contains(value)).cloned().collect();
    (candidates, residual)
}

/// ソート済みの非NULL値`sorted_values`から、等頻度Histogramを組み立てる。
///
/// [`HISTOGRAM_BUCKET_COUNT`]個のバケツに、行数ができるだけ均等になるよう
/// 分割する。値が1件も無ければ空の`Vec`を返す。値の総数が
/// `HISTOGRAM_BUCKET_COUNT`未満の場合は、その値の総数ぶんのバケツしか
/// できない(1バケツ1行になる)。
fn build_equi_depth_histogram(sorted_values: &[Value]) -> Vec<Bucket> {
    if sorted_values.is_empty() {
        return Vec::new();
    }

    let total = sorted_values.len();
    let bucket_count = total.min(HISTOGRAM_BUCKET_COUNT);
    let base_size = total / bucket_count;
    let remainder = total % bucket_count;

    let mut buckets = Vec::with_capacity(bucket_count);
    let mut start = 0;
    for i in 0..bucket_count {
        // 割り切れない分は、先頭のバケツから1行ずつ多めに配る。
        let size = base_size + usize::from(i < remainder);
        let end = start + size;
        let chunk = &sorted_values[start..end];
        buckets.push(Bucket {
            lower: chunk.first().expect("sizeは1以上").clone(),
            upper: chunk.last().expect("sizeは1以上").clone(),
            row_count: chunk.len() as u64,
        });
        start = end;
    }
    buckets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Column, DataType};

    fn schema_one_bigint() -> Schema {
        Schema::new(vec![Column::new("v", DataType::BigInt, true)])
    }

    fn collect(values: &[Value]) -> TableStats {
        let schema = schema_one_bigint();
        let mut collector = StatsCollector::new(&schema);
        for value in values {
            let tuple = Tuple::new(&schema, vec![value.clone()]).unwrap();
            collector.add_row(&tuple);
        }
        collector.finish()
    }

    #[test]
    fn row_count_matches_number_of_rows_added() {
        let stats = collect(&[Value::BigInt(1), Value::BigInt(2), Value::BigInt(3)]);
        assert_eq!(stats.row_count, 3);
    }

    #[test]
    fn distinct_count_is_exact() {
        let stats = collect(&[Value::BigInt(1), Value::BigInt(1), Value::BigInt(2), Value::BigInt(2), Value::BigInt(3)]);
        assert_eq!(stats.columns[0].distinct_count, 3);
    }

    #[test]
    fn null_count_excludes_nulls_from_distinct_and_min_max() {
        let stats = collect(&[Value::Null, Value::BigInt(5), Value::Null, Value::BigInt(1)]);
        assert_eq!(stats.columns[0].null_count, 2);
        assert_eq!(stats.columns[0].distinct_count, 2);
        assert_eq!(stats.columns[0].min, Some(Value::BigInt(1)));
        assert_eq!(stats.columns[0].max, Some(Value::BigInt(5)));
    }

    #[test]
    fn min_max_are_computed_with_compare_values() {
        let stats = collect(&[Value::BigInt(-5), Value::BigInt(100), Value::BigInt(3)]);
        assert_eq!(stats.columns[0].min, Some(Value::BigInt(-5)));
        assert_eq!(stats.columns[0].max, Some(Value::BigInt(100)));
    }

    #[test]
    fn histogram_is_empty_for_a_column_with_no_non_null_values() {
        let stats = collect(&[Value::Null, Value::Null]);
        assert!(stats.columns[0].histogram.is_empty());
        assert!(stats.columns[0].mcv.is_empty());
    }

    #[test]
    fn histogram_splits_values_into_equal_depth_buckets() {
        let values: Vec<Value> = (0..100).map(Value::BigInt).collect();
        let stats = collect(&values);
        let histogram = &stats.columns[0].histogram;
        assert_eq!(histogram.len(), HISTOGRAM_BUCKET_COUNT);
        for bucket in histogram {
            assert_eq!(bucket.row_count, 10);
        }
        assert_eq!(histogram[0].lower, Value::BigInt(0));
        assert_eq!(histogram[0].upper, Value::BigInt(9));
        assert_eq!(histogram[9].lower, Value::BigInt(90));
        assert_eq!(histogram[9].upper, Value::BigInt(99));
        assert!(stats.columns[0].mcv.is_empty(), "全値が一意ならMCVは空のまま");
    }

    #[test]
    fn skewed_distribution_moves_the_dominant_value_into_mcv() {
        // 0が90回、1〜9がそれぞれ1回ずつ出現する、値に偏りのある分布
        // (codexレビューが指摘した再現ケース)。
        let mut values = vec![Value::BigInt(0); 90];
        values.extend((1..10).map(Value::BigInt));
        let stats = collect(&values);
        let column = &stats.columns[0];

        // 平均バケツ行数は99/10=9.9。0の出現回数(90)はこれを大きく超えるため
        // MCVに採用され、Histogramには残余の9値(1〜9、各1回)だけが残る。
        assert_eq!(column.mcv, vec![(Value::BigInt(0), 90)]);
        let residual_total: u64 = column.histogram.iter().map(|b| b.row_count).sum();
        assert_eq!(residual_total, 9);
        for bucket in &column.histogram {
            assert_eq!(bucket.row_count, 1);
        }
    }

    #[test]
    fn mcv_never_exceeds_the_maximum_entry_count_even_with_many_skewed_values() {
        // 9種類の値がそれぞれ平均バケツ行数を大きく超える頻度(100回)で
        // 出現し、残りは1回だけの値がたくさんある分布。モジュール冒頭の
        // 説明のとおり、平均バケツ行数を上回る値どうしの出現回数の合計は
        // 非NULL行数を超えられないため、MCV_MAX_ENTRIES件に切り詰める分岐へ
        // 実際に到達することは無い。ここでは、その上限を超えないことだけを
        // 回帰として確認する(切り詰め自体は`crate::storage`の
        // `validate_stats_metadata`が外部入力に対して検査する)。
        let mut values = Vec::new();
        for v in 0..9 {
            values.extend(std::iter::repeat_n(Value::BigInt(v), 200));
        }
        values.extend((100..200).map(Value::BigInt));
        let stats = collect(&values);
        assert!(stats.columns[0].mcv.len() < MCV_MAX_ENTRIES, "mcv={:?}", stats.columns[0].mcv);
        assert_eq!(stats.columns[0].mcv.len(), 9);
    }

    #[test]
    fn histogram_has_fewer_buckets_than_distinct_values_available() {
        let stats = collect(&[Value::BigInt(1), Value::BigInt(2), Value::BigInt(3)]);
        assert_eq!(stats.columns[0].histogram.len(), 3);
    }

    #[test]
    fn text_and_boolean_columns_are_also_summarized() {
        let schema = Schema::new(vec![Column::new("t", DataType::Text, true), Column::new("b", DataType::Boolean, true)]);
        let mut collector = StatsCollector::new(&schema);
        for (text, boolean) in [("banana", true), ("apple", false), ("cherry", true)] {
            let tuple = Tuple::new(&schema, vec![Value::Text(text.to_string()), Value::Boolean(boolean)]).unwrap();
            collector.add_row(&tuple);
        }
        let stats = collector.finish();
        assert_eq!(stats.columns[0].min, Some(Value::Text("apple".to_string())));
        assert_eq!(stats.columns[0].max, Some(Value::Text("cherry".to_string())));
        assert_eq!(stats.columns[1].min, Some(Value::Boolean(false)));
        assert_eq!(stats.columns[1].max, Some(Value::Boolean(true)));
    }
}
