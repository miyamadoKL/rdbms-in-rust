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
//! * 列ごとのHistogram(`ColumnStats::histogram`)。バケツ数は固定
//!   ([`HISTOGRAM_BUCKET_COUNT`])で、**等頻度(equi-depth)**方式を採る
//!   (各バケツにほぼ同数の行が収まるよう境界を決める)。等幅(equi-width、
//!   値の範囲を等分する)ではなくこちらを選んだ理由は、一部の値に行が
//!   集中する歪んだ分布でも、集中している値の周辺だけバケツが密になる
//!   (=範囲が狭くなる)ことで精度が落ちにくいためである。PostgreSQLの
//!   `ANALYZE`が作る`pg_stats.histogram_bounds`も等頻度方式を採っている。
//!
//! `Min`/`Max`/`Histogram`の対象は、[`crate::types::compare_values`]で
//! 比較できる列すべて(`BOOLEAN`・`BIGINT`・`TEXT`のいずれも)である。

use std::collections::HashSet;

use crate::types::{Schema, Tuple, Value, compare_values};

/// Histogramのバケツ数(固定)。
pub const HISTOGRAM_BUCKET_COUNT: usize = 10;

/// Histogramの1バケツ。`[lower, upper]`(両端を含む)の範囲に、`row_count`件の
/// 非NULL値が収まっている。
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
    /// この列のDistinct値数(`NULL`を除く、正確な値)。
    pub distinct_count: u64,
    /// この列の最小値(`NULL`を除く)。値が1件も無ければ`None`。
    pub min: Option<Value>,
    /// この列の最大値(`NULL`を除く)。
    pub max: Option<Value>,
    /// 等頻度Histogram。値が1件も無ければ空の`Vec`。
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
/// `StatsCollector::finish`がHistogramを組み立てて[`ColumnStats`]へ変換する。
#[derive(Debug, Default)]
struct ColumnAccumulator {
    null_count: u64,
    distinct: HashSet<Value>,
    min: Option<Value>,
    max: Option<Value>,
    /// 非NULLの値を出現順のまま集めておく(Histogram組み立て用)。
    /// `finish`の時点でソートしてから等頻度に分割する。
    values: Vec<Value>,
}

/// テーブルを1回走査しながら[`TableStats`]を組み立てる集計器。
///
/// `ANALYZE`(第27章)は、`SeqScan`相当の全件走査(`Database::build_query_executor`
/// が組み立てる`Executor`)から1行ずつ`Tuple`を受け取り、その都度
/// `add_row`を呼ぶ。最後に`finish`を呼ぶと、Histogramの境界を確定させた
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

    /// 集計を締め、Histogramを組み立てた[`TableStats`]を返す。
    pub fn finish(self) -> TableStats {
        let columns = self
            .columns
            .into_iter()
            .map(|mut accumulator| {
                accumulator.values.sort_by(compare_values);
                let histogram = build_equi_depth_histogram(&accumulator.values);
                ColumnStats {
                    null_count: accumulator.null_count,
                    distinct_count: accumulator.distinct.len() as u64,
                    min: accumulator.min,
                    max: accumulator.max,
                    histogram,
                }
            })
            .collect();
        TableStats { row_count: self.row_count, columns }
    }
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
    }

    #[test]
    fn histogram_stays_equal_depth_under_a_skewed_distribution() {
        // 0が90回、1〜9がそれぞれ1回ずつ出現する、値に偏りのある分布。
        let mut values = vec![Value::BigInt(0); 90];
        values.extend((1..10).map(Value::BigInt));
        let stats = collect(&values);
        let histogram = &stats.columns[0].histogram;
        // 等幅Histogramなら、値0のバケツに90行すべてが押し込まれる。
        // 等頻度Histogramでは、行数はどのバケツもほぼ均等(9または10)になる。
        for bucket in histogram {
            assert!(bucket.row_count == 9 || bucket.row_count == 10);
        }
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
