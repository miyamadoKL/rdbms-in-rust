//! [`crate::statistics::TableStats`]から選択率・行数を見積もる推定式(第27章)。
//!
//! この章の推定式は、`EXPLAIN`/`EXPLAIN ANALYZE`が表示する参考値としてのみ
//! 使う。`physical_plan::optimize`のアクセスパス・Join方式の選択ロジックは
//! この章では変更しない(選択にコストを使い始めるのは第28章のCost Model)。
//!
//! # 選択率の意味: 全行のうち述語がTRUEになる行の割合
//!
//! この章の選択率は、一貫して「対象範囲の全行(NULLを含む)のうち、述語が
//! `TRUE`と評価される行の割合」を表す。SQLの3値論理(`TRUE`/`FALSE`/`UNKNOWN`)
//! では、`NULL`を含む比較は`UNKNOWN`になり、`WHERE`句は`UNKNOWN`の行を
//! `FALSE`と同じく落とす。したがって選択率は「非NULL行のうち一致する割合」
//! ではなく、「全行のうちTRUEになる割合」でなければならない。
//! `null_count`/`row_count`から求めた**NULL率**(欠損している行の割合)を
//! 使い、等値・範囲述語は「非NULLである割合」×「非NULL行内での選択率」の
//! 積として見積もる(詳細は[`estimate_equality_selectivity`]、
//! [`estimate_range_selectivity`]を参照)。
//!
//! # `AND`/`OR`/`NOT`は3値論理の確率(`Selectivity3`)で合成する
//!
//! `col <op> 定数`のような葉の述語は`TRUE`の1値(選択率)だけで表せるが、
//! `AND`・`OR`・`NOT`を含む複合述語はそれでは正しく合成できない
//! (`FALSE AND UNKNOWN`は`FALSE`、`TRUE OR UNKNOWN`は`TRUE`と確定するなど、
//! `UNKNOWN`の伝播規則が単純な積や補数では再現できないため、第4部2巡目
//! レビュー対応)。そのためこの章は、`TRUE`/`FALSE`/`UNKNOWN`の3確率を持つ
//! [`Selectivity3`]を葉から再帰的に組み立て、[`and3`]・[`or3`]・[`not3`]で
//! SQLの真理値表どおりに合成する。呼び出し側(`physical_plan::predicate_selectivity`)
//! が式木をたどって`Selectivity3`を組み立て、最後に`is_true`だけを取り出す。
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

/// SQLの3値論理(`TRUE`/`FALSE`/`UNKNOWN`)における、1個の述語の推定確率
/// (第4部2巡目レビュー対応)。
///
/// `is_true + is_false + is_unknown == 1.0`(丸め誤差を除く)を保つ。
/// `WHERE`句が拾うのは`is_true`の行だけだが、`AND`・`OR`・`NOT`を正しく
/// 合成するには`TRUE`の確率だけでは足りない。`FALSE AND UNKNOWN`は
/// `FALSE`、`TRUE OR UNKNOWN`は`TRUE`と確定するため、「両辺が`UNKNOWN`で
/// ない割合」のような単純な指標だけでは、この確定規則を再現できない
/// (`crate::physical_plan`が第1巡目のレビュー対応で使っていた
/// `known_fraction`方式の限界。詳細は[`and3`]・[`or3`]のドキュメントを参照)。
/// この構造体は、比較・`IS [NOT] NULL`という葉から`TRUE`/`FALSE`/`UNKNOWN`の
/// 3確率を計算し、`AND`・`OR`・`NOT`をSQLの真理値表どおりに合成すること
/// (葉どうしの独立性は仮定する)で、この確定規則を再現する。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Selectivity3 {
    pub is_true: f64,
    pub is_false: f64,
    pub is_unknown: f64,
}

impl Selectivity3 {
    /// `UNKNOWN`にならない(`is_unknown == 0.0`)述語。`IS [NOT] NULL`など、
    /// `NULL`を渡しても`TRUE`/`FALSE`のどちらかに定まる述語に使う。
    pub fn certain(is_true: f64) -> Self {
        let is_true = is_true.clamp(0.0, 1.0);
        Selectivity3 { is_true, is_false: 1.0 - is_true, is_unknown: 0.0 }
    }

    /// `is_true`・`is_unknown`の2つから組み立てる。`is_false`は残りとして
    /// 求める(3つの合計が1.0になるように)。`col <op> 定数`のような比較述語
    /// (`UNKNOWN`になるのは被演算子が`NULL`のときだけ)に使う。
    pub fn from_true_and_unknown(is_true: f64, is_unknown: f64) -> Self {
        let is_true = is_true.clamp(0.0, 1.0);
        let is_unknown = is_unknown.clamp(0.0, 1.0 - is_true);
        Selectivity3 { is_true, is_false: (1.0 - is_true - is_unknown).max(0.0), is_unknown }
    }

    /// 常に`UNKNOWN`(`v = NULL`のような、比較の一方が`NULL`リテラルの述語)。
    pub fn always_unknown() -> Self {
        Selectivity3 { is_true: 0.0, is_false: 0.0, is_unknown: 1.0 }
    }
}

/// `null_count`件が`row_count`行中のNULLである列の、NULL率(0.0〜1.0)。
///
/// `row_count`が0の場合(空テーブル)は0.0を返す。`null_count`が`row_count`を
/// 超える(壊れた統計)場合は1.0に丸める。この丸めは`estimate_*`系の関数を
/// 呼ぶ側(`Storage::open`・`Storage::set_table_stats`)で検証しきれない
/// 呼び出し経路(テストが手作りした`ColumnStats`など)のための保険であり、
/// 通常の経路では`validate_stats_metadata`(第4部レビュー対応)がこの不整合を
/// 事前に弾く。
pub fn null_fraction(null_count: u64, row_count: u64) -> f64 {
    if row_count == 0 {
        return 0.0;
    }
    (null_count as f64 / row_count as f64).clamp(0.0, 1.0)
}

/// 等値述語(`col = value`)の選択率を見積もる。全行(NULLを含む)のうち
/// `TRUE`になる行の割合を返す。
///
/// `value`自身が`NULL`(`v = NULL`のような式)なら、SQLの3値論理でこの比較は
/// 常に`UNKNOWN`になり、`WHERE`句が拾う`TRUE`の行は存在しないため0.0を返す。
///
/// それ以外は、「列が非NULLである割合」×「非NULL行の中での一致割合」の積で
/// 見積もる。「非NULL行の中での一致割合」は、HistogramがあればMCV
/// (最頻値、[`crate::statistics::ColumnStats::mcv`])に一致すればその実頻度、
/// なければ残余のHistogramのバケツ比率をバケツ内Distinct値数で割った値、
/// Histogramが無ければ`1 / NDV`(NDVも無ければ[`DEFAULT_EQ_SEL`])を使う。
pub fn estimate_equality_selectivity(stats: Option<&ColumnStats>, row_count: u64, value: &Value) -> f64 {
    if value.is_null() {
        return 0.0;
    }
    let Some(stats) = stats else { return DEFAULT_EQ_SEL };
    let non_null = 1.0 - null_fraction(stats.null_count, row_count);
    non_null * equality_selectivity_within_non_null(stats, row_count, value)
}

/// [`estimate_equality_selectivity`]の3値論理版。[`Selectivity3`]のドキュメントを
/// 参照。`value`が`NULL`なら常に`UNKNOWN`([`Selectivity3::always_unknown`])。
/// それ以外は、`TRUE`の割合は[`estimate_equality_selectivity`]と同じ値を使い、
/// `UNKNOWN`の割合は列のNULL率([`comparison_unknown_fraction`])を使う。
pub fn equality_selectivity3(stats: Option<&ColumnStats>, row_count: u64, value: &Value) -> Selectivity3 {
    if value.is_null() {
        return Selectivity3::always_unknown();
    }
    let is_true = estimate_equality_selectivity(stats, row_count, value);
    let is_unknown = comparison_unknown_fraction(stats, row_count);
    Selectivity3::from_true_and_unknown(is_true, is_unknown)
}

/// `col <op> value`という比較述語が`UNKNOWN`になる行の割合。`UNKNOWN`になる
/// のは、この章の式体系では被演算子の列が`NULL`のときだけである(`value`
/// 自身が`NULL`のケースは呼び出し側が先に判定し、常に`UNKNOWN`として扱う)。
/// 統計が無ければ、列のNULL率が分からないため0.0(常に非NULL)とみなす。
/// これは、統計が無いときの`estimate_equality_selectivity`等が
/// `DEFAULT_EQ_SEL`へ素通しでフォールバックする(NULL率による割引をしない)
/// ことと整合する。
fn comparison_unknown_fraction(stats: Option<&ColumnStats>, row_count: u64) -> f64 {
    match stats {
        Some(stats) => null_fraction(stats.null_count, row_count),
        None => 0.0,
    }
}

/// [`estimate_equality_selectivity`]の内部計算。「非NULL行の中で`value`に
/// 一致する行の割合」(0.0〜1.0)を返す。呼び出し側で「列が非NULLである割合」
/// を掛けることで、全行基準の選択率になる。
fn equality_selectivity_within_non_null(stats: &ColumnStats, row_count: u64, value: &Value) -> f64 {
    let non_null_rows = (row_count.saturating_sub(stats.null_count)) as f64;
    if non_null_rows <= 0.0 {
        return DEFAULT_EQ_SEL;
    }

    // MCV(最頻値)に載っている値は、Histogramより先に実頻度で答える
    // (`crate::statistics`モジュールの説明を参照)。
    if let Some(&(_, count)) = stats.mcv.iter().find(|(v, _)| v == value) {
        return (count as f64 / non_null_rows).clamp(0.0, 1.0);
    }

    if !stats.histogram.is_empty() {
        // Histogramは、MCVに載った値を除いた残余の非NULL値だけで組み立てて
        // ある(`crate::statistics::StatsCollector::finish`)。残余のNDVは
        // 全体のNDVからMCVの件数を引いたものである。
        let residual_ndv = (stats.distinct_count.saturating_sub(stats.mcv.len() as u64)).max(1) as f64;
        let bucket_count = stats.histogram.len() as f64;
        let ndv_per_bucket = (residual_ndv / bucket_count).max(1.0);
        for bucket in &stats.histogram {
            if compare_values(value, &bucket.lower) != Ordering::Less && compare_values(value, &bucket.upper) != Ordering::Greater
            {
                // `build_equi_depth_histogram`(`crate::statistics`)は、同じ値の
                // 連続runをバケツ境界で分割しない。したがって`value`を含む
                // バケツは必ずちょうど1個であり、複数のバケツにまたがって
                // 合算する必要は無い(第4部3巡目レビュー対応。以前の実装は
                // 単一値バケツどうしの合算しか行っておらず、単一値バケツと
                // 混合バケツにまたがる場合を数え落としていた)。
                if compare_values(&bucket.lower, &bucket.upper) == Ordering::Equal {
                    // このバケツの中身は`value`だけ(単一値バケツ)なので、
                    // 実際の行数をそのまま使える。
                    return (bucket.row_count as f64 / non_null_rows).clamp(0.0, 1.0);
                }
                // 混合バケツ(複数のDistinct値が入っている)。バケツ内の行が
                // 均等にDistinct値へ散らばっているとみなし、1つの値あたりの
                // 行数を求める。分母は非NULL行全体(残余だけではない)なので、
                // ここで直接「非NULL行の中での割合」になる。
                return (bucket.row_count as f64 / ndv_per_bucket / non_null_rows).clamp(0.0, 1.0);
            }
        }
        // どのバケツにも収まらない(観測範囲の外の値)。稀な値として扱う。
        return DEFAULT_EQ_SEL;
    }

    if !stats.mcv.is_empty() {
        // Histogramは空だが、MCVは非空。`extract_mcv`はHistogramに残余の
        // 値が1つでも残っていれば必ず1個以上のバケツを作る
        // (`crate::statistics::build_equi_depth_histogram`)ため、Histogramが
        // 空ということは残余が空、つまりMCVが非NULLの値をすべて網羅して
        // いることを意味する。`value`はここまでにMCVへ見つからなかった
        // (このifより前の`if let Some(...)`を参照)ので、`value`はこの列に
        // 一度も出現していないと確定できる(第4部2巡目レビュー対応、
        // codexの再現: `a`が常に`0`の列に対する`a = 1`が、統計上は稀では
        // なく確実に0行のはずが、次の`1 / distinct_count`という単純な
        // NDVフォールバックのせいで実際には無視できない値を返していた)。
        return 0.0;
    }

    // MCVもHistogramも無い(Distinct値数しか分からない)場合だけ、
    // 「NDV個の値が一様に分布している」という最も粗い仮定にフォールバックする。
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
/// バケツ全体が満たさない(0.0)」の2通りは、それ以上の情報を使わずに確定する。
/// 片方だけ満たす場合は、`value`がBIGINTなら`(value - lower) / (upper - lower)`
/// による線形補間(バケツ内で値が一様に分布しているとみなし、区間のどの
/// 位置に`value`があるかで按分する)、TEXT・BOOLEANのように差が定義できない
/// 型では中点(0.5)を仮定する(本文で理由を説明する)。
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
        _ => {
            // バケツの内部に境界がある。`value`がバケツのどの位置にあるかを、
            // 「lowerからvalueまでの距離」÷「lowerからupperまでの距離」の
            // 割合として求め、満たす側(`satisfies(upper)`かどうか)に応じて
            // その割合か、その補数を返す。
            let position = linear_interpolation_position(value, lower, upper);
            match position {
                Some(position) => {
                    if satisfies(upper) {
                        // upper側が満たす: valueより上の部分が対象。
                        (1.0 - position).clamp(0.0, 1.0)
                    } else {
                        // lower側が満たす: valueより下の部分が対象。
                        position.clamp(0.0, 1.0)
                    }
                }
                None => 0.5,
            }
        }
    }
}

/// `value`が区間`[lower, upper]`のどの位置にあるかを`0.0`(`lower`)〜`1.0`
/// (`upper`)の比率で返す、BIGINT専用の線形補間。
///
/// `lower == upper`(区間の幅が0)、または`BIGINT`以外の型(TEXT・BOOLEAN)は
/// `None`を返す。TEXT・BOOLEANは`compare_values`による大小比較(全順序)は
/// 持つが、2値の「距離」(差)を定義する演算を持たない。`"apple"`と`"banana"`の
/// 間に`"apricot"`がどれだけ近いかを測る自然な数値は存在しないため、この章では
/// 中点(0.5)という一様分布の仮定にとどめる。
///
/// `v - lo`・`hi - lo`を`i64`のまま引き算すると、`lo`が`i64::MIN`に近く
/// `v`・`hi`が大きい(または符号が逆)場合に有効な値どうしの差でも`i64`の
/// 表現範囲を超え、デバッグビルドでは`attempt to subtract with overflow`で
/// 停止し、リリースビルドでは巻き戻った不正な値を静かに使ってしまう
/// (第4部2巡目レビュー対応)。`i64`の差は最大で`u64::MAX`(2^64-1)に達しうる
/// ため、一旦`i128`へ拡張してから引き算し、その結果を`f64`へ変換することで
/// この範囲のどんな`i64`どうしの差も正確に計算する。
fn linear_interpolation_position(value: &Value, lower: &Value, upper: &Value) -> Option<f64> {
    match (value, lower, upper) {
        (Value::BigInt(v), Value::BigInt(lo), Value::BigInt(hi)) if hi != lo => {
            let v = i128::from(*v);
            let lo = i128::from(*lo);
            let hi = i128::from(*hi);
            let position = (v - lo) as f64 / (hi - lo) as f64;
            Some(position.clamp(0.0, 1.0))
        }
        _ => None,
    }
}

/// 範囲述語(`col > / >= / < / <= value`)の選択率を見積もる。全行(NULLを
/// 含む)のうち`TRUE`になる行の割合を返す。
///
/// `value`が`NULL`なら、等値述語と同じ理由([`estimate_equality_selectivity`]
/// を参照)で常に`UNKNOWN`になるため0.0を返す。それ以外は「列が非NULLである
/// 割合」×「非NULL行の中での選択率」の積で見積もる。Histogramがあれば
/// バケツごとの[`bucket_overlap_fraction`]を行数で重み付けして合計する。
/// 無ければMin/Maxを1個のバケツとみなして同じ計算を行う。Min/Maxも無ければ
/// [`DEFAULT_INEQ_SEL`]にフォールバックする。
pub fn estimate_range_selectivity(stats: Option<&ColumnStats>, row_count: u64, op: RangeOp, value: &Value) -> f64 {
    if value.is_null() {
        return 0.0;
    }
    let Some(stats) = stats else { return DEFAULT_INEQ_SEL };
    let non_null = 1.0 - null_fraction(stats.null_count, row_count);
    non_null * range_selectivity_within_non_null(stats, op, value)
}

/// [`estimate_range_selectivity`]の3値論理版。[`equality_selectivity3`]と
/// 同じ考え方で、`TRUE`の割合は[`estimate_range_selectivity`]と同じ値、
/// `UNKNOWN`の割合は列のNULL率を使う。
pub fn range_selectivity3(stats: Option<&ColumnStats>, row_count: u64, op: RangeOp, value: &Value) -> Selectivity3 {
    if value.is_null() {
        return Selectivity3::always_unknown();
    }
    let is_true = estimate_range_selectivity(stats, row_count, op, value);
    let is_unknown = comparison_unknown_fraction(stats, row_count);
    Selectivity3::from_true_and_unknown(is_true, is_unknown)
}

/// [`estimate_range_selectivity`]の内部計算。「非NULL行の中で述語を満たす
/// 行の割合」(0.0〜1.0)を返す。
fn range_selectivity_within_non_null(stats: &ColumnStats, op: RangeOp, value: &Value) -> f64 {
    // Histogramは残余(MCVを除いた)非NULL値だけを持つため、非NULL行全体に
    // 対する割合を求めるには、MCVの各値についても個別に判定してから合算する
    // 必要がある。MCVは実際の値そのものを持つため、`satisfies_range`で
    // 過不足なく正確に判定できる(近似が要るのはHistogramのバケツだけ)。
    let histogram_rows: u64 = stats.histogram.iter().map(|b| b.row_count).sum();
    let mcv_rows: u64 = stats.mcv.iter().map(|(_, count)| count).sum();
    let total_rows = histogram_rows + mcv_rows;

    if total_rows == 0 {
        // MCVもHistogramも無い(Distinct値数やMin/Maxしか分からない)場合。
        // Min/Maxを1個のバケツとみなした近似にフォールバックする。
        return match (&stats.min, &stats.max) {
            (Some(min), Some(max)) => bucket_overlap_fraction(op, value, min, max),
            _ => DEFAULT_INEQ_SEL,
        };
    }

    let histogram_matched: f64 =
        stats.histogram.iter().map(|b| b.row_count as f64 * bucket_overlap_fraction(op, value, &b.lower, &b.upper)).sum();
    let mcv_matched: f64 = stats
        .mcv
        .iter()
        .map(|(v, count)| if satisfies_range(op, v, value) { *count as f64 } else { 0.0 })
        .sum();
    ((histogram_matched + mcv_matched) / total_rows as f64).clamp(0.0, 1.0)
}

fn satisfies_range(op: RangeOp, x: &Value, value: &Value) -> bool {
    let ord = compare_values(x, value);
    match op {
        RangeOp::Gt => ord == Ordering::Greater,
        RangeOp::Ge => ord != Ordering::Less,
        RangeOp::Lt => ord == Ordering::Less,
        RangeOp::Le => ord != Ordering::Greater,
    }
}

/// `AND`(2つの述語の連言)の選択率。独立性を仮定した積で見積もる。
///
/// この関数は`TRUE`の1値だけを合成する、[`RangeOp`]の下限・上限のように
/// 「両辺とも`UNKNOWN`になりえない」ことが分かっている場面
/// (`physical_plan::range_selectivity_of_bounds`)専用である。一般の`WHERE`句
/// (`AND`の両辺が任意の述語になりうる)は、`UNKNOWN`の伝播規則がこの単純な
/// 積では正しく求まらないため、[`and3`]を使う([`Selectivity3`]のドキュメント、
/// および[`and3`]自身のドキュメントを参照)。
pub fn estimate_and_selectivity(a: f64, b: f64) -> f64 {
    (a * b).clamp(0.0, 1.0)
}

/// `IS NULL`述語の選択率。全行のうち`NULL`である割合、つまり
/// [`null_fraction`]そのものを返す(この述語自体は`UNKNOWN`にならない)。
/// 統計が無ければ、既定の等値選択率([`DEFAULT_EQ_SEL`])を「NULLという
/// 1つの特定の値に一致する」ことの近似として使う。
pub fn estimate_is_null_selectivity(stats: Option<&ColumnStats>, row_count: u64) -> f64 {
    match stats {
        Some(stats) => null_fraction(stats.null_count, row_count),
        None => DEFAULT_EQ_SEL,
    }
}

/// `IS NOT NULL`述語の選択率。`IS NULL`と`IS NOT NULL`は(`UNKNOWN`を経由
/// せず)全行をちょうど2つに分けるため、単純な補数`1 - IS NULLの選択率`で
/// 正確に求まる。
pub fn estimate_is_not_null_selectivity(stats: Option<&ColumnStats>, row_count: u64) -> f64 {
    1.0 - estimate_is_null_selectivity(stats, row_count)
}

/// `IS NULL`/`IS NOT NULL`述語の[`Selectivity3`]。この述語自体は`UNKNOWN`に
/// ならない([`Selectivity3::certain`])。
pub fn is_null_selectivity3(stats: Option<&ColumnStats>, row_count: u64) -> Selectivity3 {
    Selectivity3::certain(estimate_is_null_selectivity(stats, row_count))
}

/// [`is_null_selectivity3`]の`IS NOT NULL`版。
pub fn is_not_null_selectivity3(stats: Option<&ColumnStats>, row_count: u64) -> Selectivity3 {
    Selectivity3::certain(estimate_is_not_null_selectivity(stats, row_count))
}

/// `AND`(2つの述語の連言)の[`Selectivity3`]を、SQLの3値論理の真理値表どおりに
/// 合成する(第4部2巡目レビュー対応)。
///
/// `AND`が`FALSE`になるのは「どちらか一方が`FALSE`」のときであり、他方が
/// `UNKNOWN`でも構わない(`FALSE AND UNKNOWN`は`FALSE`)。したがって
/// `P(AND=FALSE)`は、`a`・`b`の`FALSE`という事象の独立性を仮定した包除原理
/// `P(a=F) + P(b=F) - P(a=F)P(b=F)`で求める。`AND`が`TRUE`になるのは
/// 「両方とも`TRUE`」のときに限られるため、`P(AND=TRUE) = P(a=T)P(b=T)`
/// (独立性の仮定のもとでの積)である。`P(AND=UNKNOWN)`は残りの確率
/// (`1 - TRUE - FALSE`)として求める。
///
/// 第1巡目のレビュー対応で採用していた`known_fraction`方式(`physical_plan`の
/// 旧`operand_non_null_fraction`)は、「両辺が非`UNKNOWN`である割合」の積で
/// `NOT`の分母を近似していた。この方式は`FALSE AND UNKNOWN`のように、
/// 片方が`UNKNOWN`でも結果が確定するケースを見逃し、確定するはずの行まで
/// `UNKNOWN`として扱ってしまう欠陥があった(第4部2巡目レビューの再現:
/// `NOT (a = 1 AND b = 1)`で`a`が常に`FALSE`、`b`が常に`UNKNOWN`のとき、
/// 内側の`AND`は全行`FALSE`のはずが、旧方式は`known_fraction`を0近くまで
/// 引き下げてしまい、`NOT`後の見積もりを実際より小さくしていた)。この
/// 関数はその欠陥を修正し、旧`known_fraction`方式・`estimate_not_selectivity`
/// を置き換える。
pub fn and3(a: Selectivity3, b: Selectivity3) -> Selectivity3 {
    let is_true = (a.is_true * b.is_true).clamp(0.0, 1.0);
    let is_false = (a.is_false + b.is_false - a.is_false * b.is_false).clamp(0.0, 1.0);
    let is_unknown = (1.0 - is_true - is_false).max(0.0);
    Selectivity3 { is_true, is_false, is_unknown }
}

/// `OR`(2つの述語の選言)の[`Selectivity3`]を、SQLの3値論理の真理値表どおりに
/// 合成する(第4部2巡目レビュー対応)。[`and3`]のドキュメントを参照。
///
/// `OR`が`TRUE`になるのは「どちらか一方が`TRUE`」のときであり
/// (`TRUE OR UNKNOWN`は`TRUE`)、独立性を仮定した包除原理
/// `P(a=T) + P(b=T) - P(a=T)P(b=T)`で求める。`OR`が`FALSE`になるのは
/// 「両方とも`FALSE`」のときに限られるため、`P(OR=FALSE) = P(a=F)P(b=F)`
/// (独立性の仮定のもとでの積)である。
pub fn or3(a: Selectivity3, b: Selectivity3) -> Selectivity3 {
    let is_true = (a.is_true + b.is_true - a.is_true * b.is_true).clamp(0.0, 1.0);
    let is_false = (a.is_false * b.is_false).clamp(0.0, 1.0);
    let is_unknown = (1.0 - is_true - is_false).max(0.0);
    Selectivity3 { is_true, is_false, is_unknown }
}

/// `NOT`(否定)の[`Selectivity3`]。3値論理では、`NOT`は`TRUE`と`FALSE`を
/// 入れ替え、`UNKNOWN`はそのまま`UNKNOWN`にとどまる(`NOT(UNKNOWN)`も
/// `UNKNOWN`)。
pub fn not3(p: Selectivity3) -> Selectivity3 {
    Selectivity3 { is_true: p.is_false, is_false: p.is_true, is_unknown: p.is_unknown }
}

/// 等値結合(`left.k = right.k`)の結果行数を見積もる、標準的な式。
///
/// `|L| × |R| / max(NDV_l, NDV_r)`。結合列がどちらも一意キー(NDVが行数と
/// 等しい)であれば、この式は「外側の各行に対して内側がちょうど1行だけ
/// 一致する」という主キー・外部キー結合の典型的な状況に一致する。
/// NDVが分からない列は、`left_ndv`・`right_ndv`にそれぞれの行数
/// (「値はすべて一意」という最も楽観的な既定値)を渡すことを想定する。
/// `left_rows`・`right_rows`には、結合キー列が`NULL`の行を含めない
/// (`NULL`同士は等号で一致しない)。呼び出し側で列の非NULL行数を渡すことを
/// 想定する(`physical_plan::apply_non_null_fraction`を参照)。
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
            mcv: Vec::new(),
        }
    }

    fn row_count_of(stats: &ColumnStats) -> u64 {
        stats.null_count + stats.histogram.iter().map(|b| b.row_count).sum::<u64>() + stats.mcv.iter().map(|(_, c)| c).sum::<u64>()
    }

    #[test]
    fn equality_without_stats_uses_default_selectivity() {
        assert_eq!(estimate_equality_selectivity(None, 1000, &Value::BigInt(1)), DEFAULT_EQ_SEL);
    }

    #[test]
    fn equality_against_null_literal_is_always_zero() {
        let stats = ColumnStats { null_count: 0, distinct_count: 4, min: None, max: None, histogram: Vec::new(), mcv: Vec::new() };
        assert_eq!(estimate_equality_selectivity(Some(&stats), 4, &Value::Null), 0.0);
        assert_eq!(estimate_equality_selectivity(None, 4, &Value::Null), 0.0);
    }

    #[test]
    fn equality_without_histogram_uses_inverse_of_ndv() {
        let stats = ColumnStats { null_count: 0, distinct_count: 4, min: None, max: None, histogram: Vec::new(), mcv: Vec::new() };
        assert_eq!(estimate_equality_selectivity(Some(&stats), 4, &Value::BigInt(1)), 0.25);
    }

    #[test]
    fn equality_with_histogram_uses_matching_bucket() {
        // 10バケツ、各100行、それぞれ10個のdistinct値(全体で100distinct)を仮定する。
        let buckets: Vec<(i64, i64, u64)> = (0..10).map(|i| (i * 10, i * 10 + 9, 100)).collect();
        let stats = stats_with_histogram(100, buckets);
        let row_count = row_count_of(&stats);
        // 値55はバケツ[50,59]に入る。バケツの行割合=100/1000=0.1、
        // バケツ内distinct数の近似=100/10=10。選択率=0.1/10=0.01。
        let selectivity = estimate_equality_selectivity(Some(&stats), row_count, &Value::BigInt(55));
        assert!((selectivity - 0.01).abs() < 1e-9, "selectivity={selectivity}");
    }

    #[test]
    fn equality_outside_histogram_range_falls_back_to_default() {
        let stats = stats_with_histogram(10, vec![(0, 9, 10)]);
        let row_count = row_count_of(&stats);
        assert_eq!(estimate_equality_selectivity(Some(&stats), row_count, &Value::BigInt(999)), DEFAULT_EQ_SEL);
    }

    #[test]
    fn equality_is_zero_when_mcv_exhausts_the_column_and_the_value_is_absent() {
        // 列の非NULL値がすべて単一の値(0)で、その値だけでMCVの閾値を
        // 超えるため残余のHistogramが空になる場合(第4部2巡目レビュー対応、
        // `three_valued_logic_table`と同じ状況)。MCVはこの列の非NULL値を
        // すべて網羅しているため、MCVに無い値(1)の選択率は「NDVの逆数」
        // ではなく確実に0であるべき。
        let stats = ColumnStats {
            null_count: 0,
            distinct_count: 1,
            min: Some(Value::BigInt(0)),
            max: Some(Value::BigInt(0)),
            mcv: vec![(Value::BigInt(0), 100)],
            histogram: Vec::new(),
        };
        assert_eq!(estimate_equality_selectivity(Some(&stats), 100, &Value::BigInt(1)), 0.0);
    }

    #[test]
    fn range_uses_mcv_when_histogram_is_empty() {
        // 上と同じ、MCVが列全体を網羅する分布。範囲述語もMCVの実際の値を
        // 使って正確に判定できるはずで、Min/Maxを1バケツとみなす粗い近似
        // (中点0.5)にフォールバックしてはならない。
        let stats = ColumnStats {
            null_count: 0,
            distinct_count: 1,
            min: Some(Value::BigInt(0)),
            max: Some(Value::BigInt(0)),
            mcv: vec![(Value::BigInt(0), 100)],
            histogram: Vec::new(),
        };
        // 0 < 5は常にTRUEのはず。
        assert_eq!(estimate_range_selectivity(Some(&stats), 100, RangeOp::Lt, &Value::BigInt(5)), 1.0);
        // 0 > 5は常にFALSEのはず。
        assert_eq!(estimate_range_selectivity(Some(&stats), 100, RangeOp::Gt, &Value::BigInt(5)), 0.0);
    }

    #[test]
    fn equality_uses_mcv_frequency_when_the_value_is_a_most_common_value() {
        // 0が90回、1〜9がそれぞれ1回ずつ出現する分布(codexレビューの再現ケース)。
        // 0はMCVに載り、実頻度(90/99)がそのまま返る。
        let mcv = vec![(Value::BigInt(0), 90)];
        let residual_buckets: Vec<Bucket> =
            (1..10).map(|i| Bucket { lower: Value::BigInt(i), upper: Value::BigInt(i), row_count: 1 }).collect();
        let stats = ColumnStats { null_count: 0, distinct_count: 10, min: Some(Value::BigInt(0)), max: Some(Value::BigInt(9)), histogram: residual_buckets, mcv };
        let selectivity = estimate_equality_selectivity(Some(&stats), 99, &Value::BigInt(0));
        assert!((selectivity - 90.0 / 99.0).abs() < 1e-9, "selectivity={selectivity}");
    }

    #[test]
    fn equality_uses_the_single_bucket_a_narrowly_missed_mcv_value_lands_in() {
        // 20行中2行だけが値`1`(残り18行は値`0`でMCVへ移る)。平均バケツ行数は
        // 20/10=2で、値`1`の出現回数(2)はこれを上回らないためMCVには載らない。
        // `build_equi_depth_histogram`(`crate::statistics`)は同じ値の連続run
        // をバケツ境界で分割しないため、値`1`の2行はちょうど1個の単一値
        // バケツに収まる(第4部3巡目レビュー対応。以前は行数だけで機械的に
        // 分割していたため、この2行が2個の単一値バケツに分かれることが
        // あった)。
        let mcv = vec![(Value::BigInt(0), 18)];
        let residual_buckets = vec![Bucket { lower: Value::BigInt(1), upper: Value::BigInt(1), row_count: 2 }];
        let stats =
            ColumnStats { null_count: 0, distinct_count: 2, min: Some(Value::BigInt(0)), max: Some(Value::BigInt(1)), histogram: residual_buckets, mcv };
        let selectivity = estimate_equality_selectivity(Some(&stats), 20, &Value::BigInt(1));
        assert!((selectivity - 2.0 / 20.0).abs() < 1e-9, "selectivity={selectivity}");
    }


    #[test]
    fn equality_accounts_for_null_fraction() {
        // 100行中90行がNULL、残り10行が0〜9(各1件)というcodexレビューの再現ケース。
        let buckets: Vec<(i64, i64, u64)> = (0..10).map(|i| (i, i, 1)).collect();
        let mut stats = stats_with_histogram(10, buckets);
        stats.null_count = 90;
        let selectivity = estimate_equality_selectivity(Some(&stats), 100, &Value::BigInt(0));
        // 非NULL率0.1 × (非NULL内での選択率0.1) = 0.01 → 100行中1行。
        assert!((selectivity - 0.01).abs() < 1e-9, "selectivity={selectivity}");
    }

    #[test]
    fn range_without_stats_uses_default_selectivity() {
        assert_eq!(estimate_range_selectivity(None, 1000, RangeOp::Gt, &Value::BigInt(1)), DEFAULT_INEQ_SEL);
    }

    #[test]
    fn range_against_null_literal_is_always_zero() {
        let stats = ColumnStats { null_count: 0, distinct_count: 0, min: Some(Value::BigInt(0)), max: Some(Value::BigInt(100)), histogram: Vec::new(), mcv: Vec::new() };
        assert_eq!(estimate_range_selectivity(Some(&stats), 1, RangeOp::Gt, &Value::Null), 0.0);
    }

    #[test]
    fn range_with_min_max_interpolates_linearly_for_bigint() {
        let stats = ColumnStats { null_count: 0, distinct_count: 0, min: Some(Value::BigInt(0)), max: Some(Value::BigInt(100)), histogram: Vec::new(), mcv: Vec::new() };
        // value=100での`>`は、上限自身なので満たさない(overlap=0.0)。
        assert_eq!(estimate_range_selectivity(Some(&stats), 1, RangeOp::Gt, &Value::BigInt(100)), 0.0);
        // value=-1での`>`は範囲全体が満たすので1.0。
        assert_eq!(estimate_range_selectivity(Some(&stats), 1, RangeOp::Gt, &Value::BigInt(-1)), 1.0);
        // value=25での`>`は、[0,100]のうち75%の区間が対象(線形補間)。
        let selectivity = estimate_range_selectivity(Some(&stats), 1, RangeOp::Gt, &Value::BigInt(25));
        assert!((selectivity - 0.75).abs() < 1e-9, "selectivity={selectivity}");
        // value=25での`<=`は、残り25%。
        let selectivity_le = estimate_range_selectivity(Some(&stats), 1, RangeOp::Le, &Value::BigInt(25));
        assert!((selectivity_le - 0.25).abs() < 1e-9, "selectivity_le={selectivity_le}");
    }

    #[test]
    fn linear_interpolation_position_does_not_overflow_at_i64_extremes() {
        // lower=i64::MIN、upper=i64::MAXという最大幅の区間。差(u64::MAXに
        // 相当する2^64-1)はi64はおろかu64ぎりぎりでも表現できないため、
        // i128へ拡張してから引き算する(第4部2巡目レビュー対応)。
        let position = linear_interpolation_position(&Value::BigInt(0), &Value::BigInt(i64::MIN), &Value::BigInt(i64::MAX)).unwrap();
        // 0は[i64::MIN, i64::MAX]のほぼ中央(厳密には0.5よりわずかに大きい、
        // 区間の非対称性による)。
        assert!((position - 0.5).abs() < 1e-9, "position={position}");

        // valueが下限そのもの、上限そのものでも境界値としてoverflowしない。
        let at_min = linear_interpolation_position(&Value::BigInt(i64::MIN), &Value::BigInt(i64::MIN), &Value::BigInt(i64::MAX)).unwrap();
        assert!((at_min - 0.0).abs() < 1e-9, "at_min={at_min}");
        let at_max = linear_interpolation_position(&Value::BigInt(i64::MAX), &Value::BigInt(i64::MIN), &Value::BigInt(i64::MAX)).unwrap();
        assert!((at_max - 1.0).abs() < 1e-9, "at_max={at_max}");
    }

    #[test]
    fn linear_interpolation_position_does_not_overflow_when_the_interval_crosses_zero() {
        // lowerが負、upperが正の、0をまたぐ区間。
        let position = linear_interpolation_position(&Value::BigInt(-25), &Value::BigInt(-100), &Value::BigInt(100)).unwrap();
        assert!((position - 0.375).abs() < 1e-9, "position={position}");
    }

    #[test]
    fn linear_interpolation_position_is_none_when_lower_equals_upper() {
        // 区間の幅が0(バケツが単一値)なら、一様分布の仮定そのものが
        // 意味を持たないためNoneを返す(`bucket_overlap_fraction`が0.5へ
        // フォールバックする)。
        assert_eq!(linear_interpolation_position(&Value::BigInt(5), &Value::BigInt(5), &Value::BigInt(5)), None);
    }

    #[test]
    fn range_selectivity_does_not_overflow_at_i64_extremes() {
        // i64::MINを含む区間に対する範囲選択率の計算がoverflowしないことを、
        // estimate_range_selectivity経由でも確認する(codexレビューが指摘した
        // SQL経路の再現に対応する、estimator単体側の回帰)。
        let stats = ColumnStats {
            null_count: 0,
            distinct_count: 0,
            min: Some(Value::BigInt(i64::MIN)),
            max: Some(Value::BigInt(9)),
            histogram: Vec::new(),
            mcv: Vec::new(),
        };
        let selectivity = estimate_range_selectivity(Some(&stats), 1, RangeOp::Lt, &Value::BigInt(-1));
        assert!((0.0..=1.0).contains(&selectivity), "selectivity={selectivity}");
    }

    #[test]
    fn range_with_min_max_uses_midpoint_for_text() {
        let stats = ColumnStats {
            null_count: 0,
            distinct_count: 0,
            min: Some(Value::Text("a".to_string())),
            max: Some(Value::Text("z".to_string())),
            histogram: Vec::new(),
            mcv: Vec::new(),
        };
        // TEXTは距離を定義できないため、境界をまたぐ場合は中点(0.5)を仮定する。
        let selectivity = estimate_range_selectivity(Some(&stats), 1, RangeOp::Gt, &Value::Text("m".to_string()));
        assert!((selectivity - 0.5).abs() < 1e-9, "selectivity={selectivity}");
    }

    #[test]
    fn range_with_histogram_sums_matching_buckets() {
        let buckets: Vec<(i64, i64, u64)> = vec![(0, 9, 10), (10, 19, 10), (20, 29, 10)];
        let stats = stats_with_histogram(30, buckets);
        let row_count = row_count_of(&stats);
        // value=15での`>=`: バケツ[0,9]は不一致(0.0)、[10,19]は境界を含み
        // 線形補間で(19-15)/(19-10)=4/9が対象、[20,29]は全て一致(1.0)。
        // 合計=(0 + 10*4/9 + 10)/30。
        let selectivity = estimate_range_selectivity(Some(&stats), row_count, RangeOp::Ge, &Value::BigInt(15));
        let expected = (0.0 + 10.0 * (4.0 / 9.0) + 10.0) / 30.0;
        assert!((selectivity - expected).abs() < 1e-9, "selectivity={selectivity} expected={expected}");
    }

    #[test]
    fn is_null_selectivity_is_the_null_fraction() {
        let stats = ColumnStats { null_count: 90, distinct_count: 10, min: None, max: None, histogram: Vec::new(), mcv: Vec::new() };
        assert!((estimate_is_null_selectivity(Some(&stats), 100) - 0.9).abs() < 1e-9);
        assert!((estimate_is_not_null_selectivity(Some(&stats), 100) - 0.1).abs() < 1e-9);
    }

    #[test]
    fn is_null_without_stats_uses_default_equality_selectivity() {
        assert_eq!(estimate_is_null_selectivity(None, 100), DEFAULT_EQ_SEL);
        assert_eq!(estimate_is_not_null_selectivity(None, 100), 1.0 - DEFAULT_EQ_SEL);
    }

    #[test]
    fn and_multiplies_selectivities() {
        assert!((estimate_and_selectivity(0.5, 0.4) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn not3_swaps_true_and_false_and_keeps_unknown() {
        let p = Selectivity3 { is_true: 0.3, is_false: 0.5, is_unknown: 0.2 };
        let negated = not3(p);
        assert!((negated.is_true - 0.5).abs() < 1e-9);
        assert!((negated.is_false - 0.3).abs() < 1e-9);
        assert!((negated.is_unknown - 0.2).abs() < 1e-9);
    }

    #[test]
    fn and3_is_true_only_when_both_are_true() {
        let a = Selectivity3::certain(0.5);
        let b = Selectivity3::certain(0.4);
        let result = and3(a, b);
        assert!((result.is_true - 0.2).abs() < 1e-9, "is_true={}", result.is_true);
        assert!((result.is_unknown - 0.0).abs() < 1e-9, "is_unknown={}", result.is_unknown);
    }

    #[test]
    fn and3_false_and_unknown_is_false() {
        // codexレビュー2巡目の再現: `a = 1`が常にFALSE、`b = 1`が常にUNKNOWN
        // (bが常にNULL)のとき、`a = 1 AND b = 1`はSQLの3値論理で常にFALSEに
        // 確定する(FALSEはUNKNOWNに左右されない)。
        let a = Selectivity3 { is_true: 0.0, is_false: 1.0, is_unknown: 0.0 };
        let b = Selectivity3 { is_true: 0.0, is_false: 0.0, is_unknown: 1.0 };
        let result = and3(a, b);
        assert!((result.is_false - 1.0).abs() < 1e-9, "is_false={}", result.is_false);
        assert!((result.is_true - 0.0).abs() < 1e-9, "is_true={}", result.is_true);
        assert!((result.is_unknown - 0.0).abs() < 1e-9, "is_unknown={}", result.is_unknown);
    }

    #[test]
    fn and3_true_and_unknown_is_unknown() {
        let a = Selectivity3 { is_true: 1.0, is_false: 0.0, is_unknown: 0.0 };
        let b = Selectivity3 { is_true: 0.0, is_false: 0.0, is_unknown: 1.0 };
        let result = and3(a, b);
        assert!((result.is_unknown - 1.0).abs() < 1e-9, "is_unknown={}", result.is_unknown);
        assert!((result.is_true - 0.0).abs() < 1e-9, "is_true={}", result.is_true);
        assert!((result.is_false - 0.0).abs() < 1e-9, "is_false={}", result.is_false);
    }

    #[test]
    fn or3_is_false_only_when_both_are_false() {
        let a = Selectivity3::certain(0.5);
        let b = Selectivity3::certain(0.5);
        let result = or3(a, b);
        // 1 - (1-0.5)(1-0.5) = 0.75
        assert!((result.is_true - 0.75).abs() < 1e-9, "is_true={}", result.is_true);
        assert!((result.is_unknown - 0.0).abs() < 1e-9, "is_unknown={}", result.is_unknown);
    }

    #[test]
    fn or3_true_or_unknown_is_true() {
        let a = Selectivity3 { is_true: 1.0, is_false: 0.0, is_unknown: 0.0 };
        let b = Selectivity3 { is_true: 0.0, is_false: 0.0, is_unknown: 1.0 };
        let result = or3(a, b);
        assert!((result.is_true - 1.0).abs() < 1e-9, "is_true={}", result.is_true);
        assert!((result.is_unknown - 0.0).abs() < 1e-9, "is_unknown={}", result.is_unknown);
    }

    #[test]
    fn or3_false_or_unknown_is_unknown() {
        let a = Selectivity3 { is_true: 0.0, is_false: 1.0, is_unknown: 0.0 };
        let b = Selectivity3 { is_true: 0.0, is_false: 0.0, is_unknown: 1.0 };
        let result = or3(a, b);
        assert!((result.is_unknown - 1.0).abs() < 1e-9, "is_unknown={}", result.is_unknown);
        assert!((result.is_true - 0.0).abs() < 1e-9, "is_true={}", result.is_true);
        assert!((result.is_false - 0.0).abs() < 1e-9, "is_false={}", result.is_false);
    }

    #[test]
    fn not_of_false_and_unknown_is_true() {
        // `NOT (FALSE AND UNKNOWN)` = `NOT FALSE` = `TRUE`
        // (codexレビュー2巡目の`NOT (a = 1 AND b = 1)`の再現に対応する式レベルの確認)。
        let a = Selectivity3 { is_true: 0.0, is_false: 1.0, is_unknown: 0.0 };
        let b = Selectivity3 { is_true: 0.0, is_false: 0.0, is_unknown: 1.0 };
        let result = not3(and3(a, b));
        assert!((result.is_true - 1.0).abs() < 1e-9, "is_true={}", result.is_true);
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
