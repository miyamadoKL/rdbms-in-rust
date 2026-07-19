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
//! MCVへ採用する条件は「その値の出現回数が、**その時点で**残っている
//! (まだMCVへ移していない)非NULL行数から求めた平均バケツ行数を上回ること」
//! である。出現回数の多い値から順に、1件抽出するたびに残り行数を差し引き、
//! 平均バケツ行数(残り行数を、実際にHistogramが作るバケツ数
//! `min(残り行数, `[`HISTOGRAM_BUCKET_COUNT`]`)`で割った値)を**その都度
//! 再計算**する。この抽出を、残りのどの値もその時点の平均バケツ行数を
//! 超えなくなる**固定点**まで、最大[`MCV_MAX_ENTRIES`]件まで繰り返す
//! (`extract_mcv`)。
//!
//! 抽出前の全体行数から一度だけ閾値を求める設計では、支配的な値をMCVへ
//! 移したあとの残余がなお歪んでいる場合(たとえば100行が`0`×50、`1`×8、
//! `2`〜`43`×各1という分布)を見逃す。`0`(50行)をMCVへ移すと、残り50行に
//! 対する平均バケツ行数は5行まで下がり、`1`(8行)は最初の閾値(10行)を
//! 超えなくても新しい閾値(5行)は超える。固定点まで抽出することで、`1`も
//! MCVへ移り、単一値バケツへの分割(本節の問題)を避けられる
//! (第4部2巡目レビュー対応)。
//!
//! [`MCV_MAX_ENTRIES`]件に達した時点でまだ固定点に達していない(=残余に
//! なお平均バケツ行数を超える値が残っている)場合は、抽出をそこで打ち切る。
//! この場合でも、残った値がバケツをまたぐことは無い。理由は
//! `build_equi_depth_histogram`側の設計にある(第4部3巡目レビュー対応)。
//!
//! # Histogramは同値の連続runをバケツ境界で分割しない
//!
//! 当初の`build_equi_depth_histogram`は、ソート済みの残余値を単純に
//! `行数 ÷ バケツ数`ぶんずつ機械的に切り分けていた。この方式では、MCVの
//! 閾値をわずかに下回る(=MCVには採用されないが、1バケツの目標行数は
//! 超える)値が、たまたま目標の切れ目をまたいで存在すると、その値は
//! **単一値バケツ**(その値だけで埋まったバケツ)と**混合バケツ**(その値の
//! 残りと、別の値が混在するバケツ)に分割されてしまう。等値述語の推定
//! (`crate::estimator::equality_selectivity_within_non_null`)が単一値バケツ
//! どうしの合算だけで済ませていたころは、この混合バケツ側に紛れ込んだ分を
//! 数え漏らしていた(頻度`[15, 14, 12, 11, 10, 9, 8, 7, 7, 6, 6]`+一意値42件
//! という分布で、`v = 10`が`rows=5 actual=6`になった再現がこれに当たる)。
//!
//! `build_equi_depth_histogram`は、目標の切れ目が同じ値の連続runの途中に
//! 来る場合、runの終わりまで境界を伸ばす。バケツは常に「1個以上の
//! 完全なrun」の集まりになるため、ある値が2つのバケツにまたがることは
//! 構造的に起こらない(値を含むバケツは必ずちょうど1個であり、その
//! バケツが単一値だけで埋まっているか、複数の値が混在しているかのどちらか
//! である)。この結果、等値述語の推定はバケツをまたいだ合算を一切必要と
//! せず、見つかった1個のバケツだけを見ればよくなる。
//!
//! 代わりに、バケツの行数は均等ではなくなる。目標の切れ目をまたぐ長いrun
//! があるバケツは、目標行数を超えて膨らむ(runの長さがそのままそのバケツの
//! 行数になる)。これは等頻度(equi-depth)という名前が示す「バケツの行数を
//! 均等にする」という理想からの意図的な後退だが、行数の均等さそのものより
//! 「値がバケツをまたがない」ことのほうが、この章の推定式にとって重要である
//! (`crate::estimator::equality_selectivity_within_non_null`のドキュメントを
//! 参照)。

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
/// そのまま使える)。採用条件・上限件数・固定点まで抽出する理由はモジュール
/// 冒頭の説明を参照。
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
    // 出現回数の降順。同数なら値の昇順(`counts`はソート済みの値順)で決定的に揃える。
    counts.sort_by(|(value_a, count_a), (value_b, count_b)| count_b.cmp(count_a).then_with(|| compare_values(value_a, value_b)));

    // 出現回数の多い値から順に、「その時点で残っている非NULL行数」に対する
    // 平均バケツ行数(`build_equi_depth_histogram`が実際に作るバケツ数
    // `min(残り行数, HISTOGRAM_BUCKET_COUNT)`で割った値)を都度再計算しながら
    // 抽出する。`counts`は降順なので、ある値がその時点の閾値を超えなければ、
    // それ以降の値(出現回数がさらに少ない)も、これ以降の閾値(残り行数の
    // 減少に伴って単調非増加)を超えることは無い。したがって、超えない値に
    // 出会った時点で走査を打ち切ってよい(固定点に達したことを意味する)。
    let mut mcv: Vec<(Value, u64)> = Vec::new();
    let mut remaining_rows = sorted_values.len() as u64;
    for (value, count) in counts {
        if mcv.len() >= MCV_MAX_ENTRIES {
            break;
        }
        let effective_bucket_count = remaining_rows.min(HISTOGRAM_BUCKET_COUNT as u64).max(1);
        let average_bucket_size = remaining_rows as f64 / effective_bucket_count as f64;
        if count as f64 > average_bucket_size {
            remaining_rows = remaining_rows.saturating_sub(count);
            mcv.push((value, count));
        } else {
            break;
        }
    }

    if mcv.is_empty() {
        return (Vec::new(), sorted_values.to_vec());
    }

    let mcv_values: HashSet<&Value> = mcv.iter().map(|(value, _)| value).collect();
    let residual: Vec<Value> = sorted_values.iter().filter(|value| !mcv_values.contains(value)).cloned().collect();
    (mcv, residual)
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

    let mut buckets = Vec::new();
    let mut start = 0;
    let mut bucket_index = 0;
    while start < total {
        // 割り切れない分は、先頭のバケツから1行ずつ多めに配る、という目標
        // サイズ自体は従来どおり。
        let target_size = base_size + usize::from(bucket_index < remainder);
        let mut end = (start + target_size.max(1)).min(total);
        // 目標の切れ目が同じ値の連続run(同じ値が連なった範囲)の途中に
        // 来る場合、runの終わりまで境界を伸ばす。これにより、1つの値が
        // 2つのバケツにまたがることはなくなる(モジュール冒頭の説明を参照)。
        // 結果としてバケツの行数は均等ではなくなる(runが長い値のぶん、
        // そのバケツだけ目標サイズを超える)。
        while end < total && sorted_values[end] == sorted_values[end - 1] {
            end += 1;
        }
        let chunk = &sorted_values[start..end];
        buckets.push(Bucket {
            lower: chunk.first().expect("startを含むため1行以上").clone(),
            upper: chunk.last().expect("startを含むため1行以上").clone(),
            row_count: chunk.len() as u64,
        });
        start = end;
        bucket_index += 1;
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
    fn mcv_extraction_can_reach_the_maximum_entry_count_under_the_fixed_point_rule() {
        // 出現回数を段階的に下げた10個の値([20,15,12,10,9,8,7,6,5,5]、
        // 合計97行)+ 3行の一意な値、という分布。固定点方式(モジュール冒頭の
        // 説明を参照)では、値を1つ抽出するたびに残り行数に対する平均バケツ
        // 行数が下がっていくため、単発の閾値計算では抽出されないはずの
        // 値(たとえば末尾の`5`)まで抽出対象になり、MCV_MAX_ENTRIES(10)
        // ちょうどに達する。
        let mut values = Vec::new();
        for (v, count) in [20, 15, 12, 10, 9, 8, 7, 6, 5, 5].into_iter().enumerate() {
            values.extend(std::iter::repeat_n(Value::BigInt(v as i64), count));
        }
        values.extend((100..103).map(Value::BigInt));
        let stats = collect(&values);
        assert_eq!(stats.columns[0].mcv.len(), MCV_MAX_ENTRIES, "mcv={:?}", stats.columns[0].mcv);
    }

    #[test]
    fn mcv_extraction_lowers_the_threshold_after_removing_a_dominant_value() {
        // codexレビュー2巡目の再現ケース: 100行が`0`×50、`1`×8、`2`〜`43`×
        // 各1行という分布。抽出前の平均バケツ行数(100/10=10行)だけを見ると
        // `1`(8行)はMCVに入らないが、`0`(50行)を先に抽出すると残り50行の
        // 平均バケツ行数は5行に下がり、`1`はこの新しい閾値を上回るため
        // 固定点方式ではMCVに移る。
        let mut values = vec![Value::BigInt(0); 50];
        values.extend(std::iter::repeat_n(Value::BigInt(1), 8));
        values.extend((2..44).map(Value::BigInt));
        let stats = collect(&values);
        assert_eq!(stats.columns[0].mcv, vec![(Value::BigInt(0), 50), (Value::BigInt(1), 8)]);
        let residual_total: u64 = stats.columns[0].histogram.iter().map(|b| b.row_count).sum();
        assert_eq!(residual_total, 42);
    }

    #[test]
    fn histogram_does_not_split_a_value_across_a_singleton_and_a_mixed_bucket() {
        // codexレビュー3巡目の再現ケース: 出現回数[15,14,12,11,10,9,8,7,7,6,6]
        // (値0〜10)+一意値42件(合計147行)という分布。MCVは固定点方式で
        // 抽出するが、MCV_MAX_ENTRIES(10)件に達した時点で0〜9(10個)を
        // 抽出し終えており、11個目の値(`10`、出現回数6)はMCVに入らないまま
        // 残余へ回る。
        //
        // `10`を、残余に含まれる一意値(1000〜1041)よりすべて小さい値にして
        // あるため、ソート順で`10`の6行はまとまって先頭に並ぶ。旧実装
        // (行数だけを見て機械的に等頻度分割する)なら、この6行は1個目の
        // バケツ(5行、単一値)と2個目のバケツ(1行+一意値4件の混合)に
        // 分かれ、`v = 10`の推定は単一値バケツの5行しか数えられなかった
        // (`rows=5 actual=6`)。`build_equi_depth_histogram`が同値の連続run
        // をバケツ境界で分割しない今の実装では、6行すべてが1個の単一値
        // バケツに収まるはずである。
        let mut values = Vec::new();
        for (v, count) in [15, 14, 12, 11, 10, 9, 8, 7, 7, 6, 6].into_iter().enumerate() {
            values.extend(std::iter::repeat_n(Value::BigInt(v as i64), count));
        }
        values.extend((1000..1042).map(Value::BigInt));
        assert_eq!(values.len(), 147);

        let stats = collect(&values);
        let column = &stats.columns[0];

        // 0〜9(10個)がMCVへ移り、10(出現回数6)はMCV_MAX_ENTRIESの上限に
        // よって残余に残る。
        assert_eq!(column.mcv.len(), MCV_MAX_ENTRIES);
        assert!(!column.mcv.iter().any(|(v, _)| *v == Value::BigInt(10)), "mcv={:?}", column.mcv);

        let bucket_for_ten = column
            .histogram
            .iter()
            .find(|b| compare_values(&Value::BigInt(10), &b.lower) != std::cmp::Ordering::Less && compare_values(&Value::BigInt(10), &b.upper) != std::cmp::Ordering::Greater)
            .unwrap_or_else(|| panic!("値10を含むバケツが見つかりません: {:?}", column.histogram));
        assert_eq!(bucket_for_ten.lower, Value::BigInt(10));
        assert_eq!(bucket_for_ten.upper, Value::BigInt(10));
        assert_eq!(bucket_for_ten.row_count, 6, "histogram={:?}", column.histogram);
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
