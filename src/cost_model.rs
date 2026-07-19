//! 複数の物理演算子候補を比較するための、単位のないコストモデル(第28章)。
//!
//! # 実時間の予測ではなく、候補プラン同士の相対比較
//!
//! [`Cost`]はミリ秒でもページ数そのものでもない。`SEQ_PAGE_COST`・
//! `RANDOM_PAGE_COST`・`CPU_TUPLE_COST`という3つの重みを掛け合わせた
//! 無次元の合成値であり、「このクエリは何ミリ秒で終わるか」には一切
//! 答えない。答えるのは「同じ`WHERE`に対する`SeqScan`と`IndexScan`の
//! どちらが、より少ない仕事で済むか」という、候補プランどうしの大小関係
//! だけである。[`crate::physical_plan::optimize`]は、複数の候補を実際に
//! 組み立てたうえで[`plan_cost`]を呼び、最小のものを選ぶ。
//!
//! # 4要素のうちMemoryは扱わない
//!
//! `docs-local/chatgpt_opinion.md`の原案は、コストを
//! `Sequential I/O + Random I/O + CPU per Tuple + Memory`の4要素へ
//! 分解するとしている。このクレートは前半3つ(Sequential I/O、Random I/O、
//! CPU per Tuple)だけをモデル化し、Memory(作業用メモリの消費、あるいは
//! `work_mem`を超えた場合のディスクへの一時退避コスト)は重み0として
//! 明示的に無視する。
//!
//! 理由は2つある。1つは、`HashJoin`のBuild側(第22章)も`Sort`(第21章)も、
//! このクレートは常にメモリ上の`Vec`・`HashMap`へ全件を保持する実装で
//! あり、作業用メモリが不足したときにディスクへ一時ファイルを吐く経路
//! (PostgreSQLの`work_mem`超過時の外部ソート・外部ハッシュ)がそもそも
//! 存在しない。存在しない分岐のコストを見積もっても、実行時の挙動と
//! 対応しない架空の数値にしかならない。もう1つは、`crate::buffer_pool::BufferPool`
//! の固定容量(第14章)が引き起こす影響(ページの置き換えによる実質的な
//! I/O増加)は、すでにRandom I/O側の重み([`RANDOM_PAGE_COST`]、
//! `seq_page_cost`より大きい)に織り込まれている。索引経由のランダムな
//! `Storage::get`がバッファプールの置き換えを増やすという第25章の実測は、
//! 「ランダムアクセスは高くつく」という重みの差そのもので説明できるため、
//! バッファプールの容量を独立した第4の要素として扱う必要が無い。

use crate::ids::TableId;
use crate::physical_plan::{IndexNestedLoopJoinNode, IndexScanNode, PhysicalPlan, StatsLookup, estimate_rows};
use crate::storage::Storage;

// ------------------------------------------------------------------
// 重み定数: 出典はPostgreSQLの`src/backend/utils/misc/postgresql.conf.sample`
// (`seq_page_cost`・`random_page_cost`・`cpu_tuple_cost`のデフォルト値)。
// ------------------------------------------------------------------

/// 1ページぶんのSequential I/O(順読み)のコスト。PostgreSQLの
/// `seq_page_cost`のデフォルト値(1.0)を採用する。他の重みはすべて
/// この値を1単位とした相対値になる。
pub const SEQ_PAGE_COST: f64 = 1.0;

/// 1ページぶんのRandom I/O(ランダムアクセス)のコスト。PostgreSQLの
/// `random_page_cost`のデフォルト値(4.0)を採用する。回転ディスクを
/// 前提に「ランダムアクセスはシーケンシャルの4倍遅い」という経験則で
/// 決められた値だが、SSDが主流になった現在のPostgreSQLでもデフォルト
/// 値は変わっていない(索引経由のアクセスがキャッシュに乗りにくい、
/// という傾向自体はSSDでも残るため)。第25章の実測(密な結合で
/// Index Nested Loop JoinがHash Joinの15倍以上遅い)も、ランダムな
/// `Storage::get`の積み重ねが`BufferPool`の置き換えを増やすという、
/// 同じ種類の非対称性を示している。
pub const RANDOM_PAGE_COST: f64 = 4.0;

/// 1行を処理する(条件を評価する、ハッシュテーブルへ出し入れする等)
/// CPUコスト。PostgreSQLの`cpu_tuple_cost`のデフォルト値(0.01)を
/// 採用する。I/Oの重み(1.0・4.0)に比べて2桁小さく、「I/Oに比べれば
/// CPU処理は軽い」という一般的な前提を反映している。
pub const CPU_TUPLE_COST: f64 = 0.01;

/// テーブルの実ページ数が分からない(`Backend::Memory`、または[`Storage`]を
/// 参照できない場面)ときのフォールバック。1ページに収まる行数の
/// 目安として使う。`crate::page::PAGE_SIZE`(4096バイト)に対し、
/// このクレートのタプルは可変長(`TEXT`列を含みうる)で1行あたりの
/// バイト数を一律には決められないため、「小さくも大きくもない」典型的な
/// 行として50行/ページを仮定する。実ページ数が引ける場合([`Storage::table_page_count`]、
/// 第28章)は、この値を一切使わない。
pub const DEFAULT_ROWS_PER_PAGE: u64 = 50;

/// B+Treeの高さが分からない(索引の`BTree`を参照できない場面)ときの
/// フォールバック。第23章のB+Treeはノードあたり複数エントリを持つため、
/// 数千〜数万行程度のテーブルなら高さ2〜3に収まることが多い。中間的な
/// 値として3を採用する。
pub const DEFAULT_INDEX_HEIGHT: u64 = 3;

/// 単位を持たない相対コスト。
///
/// 実行時間の予測値(ミリ秒・マイクロ秒)ではない。[`SEQ_PAGE_COST`]・
/// [`RANDOM_PAGE_COST`]・[`CPU_TUPLE_COST`]という3つの重みを合成した
/// 無次元の値で、同じクエリに対する複数の候補プラン同士を比較する
/// ためだけに使う。`f64`を直接使わず`newtype`にしてあるのは、
/// 「ミリ秒として扱ってしまう」「他の`f64`の値と無自覚に混ぜてしまう」
/// といった誤用を型で防ぐためである。
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct Cost(f64);

impl Cost {
    /// コストが掛からない(候補の比較においてゼロとして扱ってよい)ことを表す。
    pub const ZERO: Cost = Cost(0.0);

    /// 生の`f64`値からコストを作る。`EXPLAIN`の表示(`cost=`の後ろに置く数値)
    /// や、テストでの数値比較のためにモジュール外へ公開する。
    pub fn new(value: f64) -> Cost {
        Cost(value)
    }

    /// 内部の`f64`値を取り出す。`EXPLAIN`の`cost=<value>`表示や、テストでの
    /// 数値比較にだけ使う。この値を実行時間として解釈してはならない
    /// (モジュール冒頭を参照)。
    pub fn value(self) -> f64 {
        self.0
    }
}

impl std::ops::Add for Cost {
    type Output = Cost;
    fn add(self, rhs: Cost) -> Cost {
        Cost(self.0 + rhs.0)
    }
}

impl std::iter::Sum for Cost {
    fn sum<I: Iterator<Item = Cost>>(iter: I) -> Cost {
        iter.fold(Cost::ZERO, |acc, c| acc + c)
    }
}

// ------------------------------------------------------------------
// 演算子ごとのコスト式
// ------------------------------------------------------------------

/// `SeqScan`のコスト。ページ数ぶんのSequential I/Oと、行数ぶんのCPUコスト
/// (各行を`decode_tuple`する処理、第13章)の和。
pub fn seq_scan_cost(pages: u64, rows: u64) -> Cost {
    Cost(pages as f64 * SEQ_PAGE_COST + rows as f64 * CPU_TUPLE_COST)
}

/// `IndexScan`のコスト。Rootから葉までの`height`段はどの段も1ページの
/// Random I/O(第23章、`BTree::height`)で、一致した`matched_rows`件は
/// それぞれ`Storage::get`によるHeapページへのRandom I/Oと、1行ぶんの
/// CPUコストを要する(第25章、`IndexScanExec::next`)。
pub fn index_scan_cost(height: u64, matched_rows: u64) -> Cost {
    let descend = height as f64 * RANDOM_PAGE_COST;
    let fetch = matched_rows as f64 * (RANDOM_PAGE_COST + CPU_TUPLE_COST);
    Cost(descend + fetch)
}

/// `Filter`・`Projection`のコスト。どちらも子から受け取った`input_rows`件
/// それぞれに対して式を1回評価するだけなので、同じ式で見積もる。
pub fn cpu_pass_cost(input_rows: u64) -> Cost {
    Cost(input_rows as f64 * CPU_TUPLE_COST)
}

/// `NestedLoopJoin`のコスト。`right`は`NestedLoopJoinExec::new`(第22章)が
/// コンストラクタで1回だけ全件読み切って`Vec<Tuple>`へ保持するため、
/// I/Oは`left`・`right`それぞれの子の[`plan_cost`]にすでに含まれる。この
/// 関数が返すのは、`left`の`left_rows`行それぞれについて`right`の
/// `right_rows`行との組み合わせを1つずつ評価するCPUコスト、
/// `|L| × |R|`である。
pub fn nested_loop_join_cost(left_rows: u64, right_rows: u64) -> Cost {
    Cost(left_rows as f64 * right_rows as f64 * CPU_TUPLE_COST)
}

/// `HashJoin`のコスト。Build(`right`の`right_rows`行をハッシュテーブルへ
/// 挿入する)とProbe(`left`の`left_rows`行それぞれでハッシュテーブルを
/// 引く)の和(第22章、`HashJoinExec::new`・`next`)。`right`・`left`の
/// 読み取り自体のI/Oは、`HashJoin`・`SeqScan`それぞれの子の[`plan_cost`]に
/// すでに含まれる。
pub fn hash_join_cost(left_rows: u64, right_rows: u64) -> Cost {
    let build = right_rows as f64 * CPU_TUPLE_COST;
    let probe = left_rows as f64 * CPU_TUPLE_COST;
    Cost(build + probe)
}

/// `IndexNestedLoopJoin`のコスト。`left`の`left_rows`行それぞれについて、
/// 内側テーブルの索引を1回`lookup`する(第25章、`IndexNestedLoopJoinExec::next`)。
/// 1回の`lookup`は[`index_scan_cost`]と同じ形("木を`height`段降りてから
/// `avg_matches`件をHeapから`fetch`する")のコストを持つが、`avg_matches`は
/// 定数ではなく「平均的な外側の値が何行と一致するか」という見積もりで
/// あるため`f64`のまま受け取る(呼び出し側は
/// `estimate_rows(IndexNestedLoopJoin) / left_rows`から求める)。
pub fn index_nested_loop_join_cost(left_rows: u64, height: u64, avg_matches: f64) -> Cost {
    let per_probe = height as f64 * RANDOM_PAGE_COST + avg_matches * (RANDOM_PAGE_COST + CPU_TUPLE_COST);
    Cost(left_rows as f64 * per_probe)
}

/// `Sort`のコスト。比較回数のオーダーである`n log n`に、比較1回ぶんの
/// CPUコストとして[`CPU_TUPLE_COST`]を掛ける。`rows`が0または1の場合は
/// 並べ替える必要が無いので`Cost::ZERO`。
pub fn sort_cost(rows: u64) -> Cost {
    if rows <= 1 {
        return Cost::ZERO;
    }
    Cost(rows as f64 * (rows as f64).log2() * CPU_TUPLE_COST)
}

// ------------------------------------------------------------------
// ページ数・索引の高さの実測値とフォールバック
// ------------------------------------------------------------------

/// `table_id`のページ数を見積もる。`storage`から実測値([`Storage::table_page_count`])
/// が引ければそれを使う(`ANALYZE`の有無に関わらず常に実測値、`Storage::table_page_count`の
/// ドキュメントを参照)。`storage`が`None`(`Backend::Memory`)、または該当
/// テーブルがまだ1ページも確保していない新規テーブルの場合は、
/// [`DEFAULT_ROWS_PER_PAGE`]から`rows`より逆算する(1ページも無くても
/// コストが0にならないよう、最低1ページとして扱う)。
fn table_pages(storage: Option<&Storage>, table_id: TableId, rows: u64) -> u64 {
    if let Some(pages) = storage.and_then(|storage| storage.table_page_count(table_id))
        && pages > 0
    {
        return pages;
    }
    rows.div_ceil(DEFAULT_ROWS_PER_PAGE).max(1)
}

/// `index_name`のB+Treeの高さを見積もる。`storage`から実際の`BTree`を引けて
/// `height()`(第23章)が成功すればその値を使う。索引を参照できない、
/// または高さの計算そのものが失敗した(壊れたページ等、通常は起こらない)
/// 場合は[`DEFAULT_INDEX_HEIGHT`]にフォールバックする。
fn index_height(storage: Option<&Storage>, index_name: &str) -> u64 {
    storage
        .and_then(|storage| storage.index_btree(index_name))
        .and_then(|btree| btree.height().ok())
        .map(|height| height as u64)
        .unwrap_or(DEFAULT_INDEX_HEIGHT)
}

// ------------------------------------------------------------------
// PhysicalPlanの木全体のコスト
// ------------------------------------------------------------------

/// `plan`の根を実行し切るまでの累積コストを見積もる。子の[`plan_cost`]を
/// 再帰的に足し合わせ、その上にこの演算子自身が追加で行う仕事のコストを
/// 乗せる。`EXPLAIN`が各行へ添える`cost=`(第28章)と、
/// [`crate::physical_plan::optimize`]が複数の候補プランから最小コストの
/// ものを選ぶ判断は、どちらもこの関数を通す。
///
/// `Values`・`Limit`・`Insert`・`Update`・`Delete`は、この章ではこの演算子
/// 自身の追加コストを0として扱う。`Values`は`ValuesExec::new`(第19章)が
/// コンストラクタ時点でSQL文の長さぶんの行しか評価しない葉であり、
/// テーブルの行数のように無視できない規模にはならない。`Limit`は子から
/// 早期に`next()`を止めるだけで、それ自体が追加の仕事をしない。
/// `Insert`・`Update`・`Delete`は`Executor`を経由しない一括処理であり
/// (`crate::physical_plan`冒頭を参照)、この章ではコストベースの選択対象にも
/// していないため、`input`の累積コストをそのまま返す。
pub fn plan_cost(plan: &PhysicalPlan, stats: &dyn StatsLookup, storage: Option<&Storage>) -> Cost {
    match plan {
        PhysicalPlan::SeqScan(scan) => {
            let rows = estimate_rows(plan, stats);
            let pages = table_pages(storage, scan.table_id, rows);
            seq_scan_cost(pages, rows)
        }
        PhysicalPlan::IndexScan(scan) => index_scan_node_cost(scan, stats, storage),
        PhysicalPlan::Values(_) => Cost::ZERO,
        PhysicalPlan::Filter(filter) => plan_cost(&filter.input, stats, storage) + cpu_pass_cost(estimate_rows(&filter.input, stats)),
        PhysicalPlan::NestedLoopJoin(join) => {
            let left_rows = estimate_rows(&join.left, stats);
            let right_rows = estimate_rows(&join.right, stats);
            plan_cost(&join.left, stats, storage) + plan_cost(&join.right, stats, storage) + nested_loop_join_cost(left_rows, right_rows)
        }
        PhysicalPlan::HashJoin(join) => {
            let left_rows = estimate_rows(&join.left, stats);
            let right_rows = estimate_rows(&join.right, stats);
            plan_cost(&join.left, stats, storage) + plan_cost(&join.right, stats, storage) + hash_join_cost(left_rows, right_rows)
        }
        PhysicalPlan::IndexNestedLoopJoin(join) => index_nested_loop_join_node_cost(join, plan, stats, storage),
        PhysicalPlan::Aggregate(aggregate) => {
            plan_cost(&aggregate.input, stats, storage) + cpu_pass_cost(estimate_rows(&aggregate.input, stats))
        }
        PhysicalPlan::Projection(projection) => {
            plan_cost(&projection.input, stats, storage) + cpu_pass_cost(estimate_rows(&projection.input, stats))
        }
        PhysicalPlan::Distinct(distinct) => {
            plan_cost(&distinct.input, stats, storage) + cpu_pass_cost(estimate_rows(&distinct.input, stats))
        }
        PhysicalPlan::Sort(sort) => plan_cost(&sort.input, stats, storage) + sort_cost(estimate_rows(&sort.input, stats)),
        PhysicalPlan::Limit(limit) => plan_cost(&limit.input, stats, storage),
        PhysicalPlan::Insert(insert) => plan_cost(&insert.input, stats, storage),
        PhysicalPlan::Update(update) => plan_cost(&update.input, stats, storage),
        PhysicalPlan::Delete(delete) => plan_cost(&delete.input, stats, storage),
    }
}

fn index_scan_node_cost(scan: &IndexScanNode, stats: &dyn StatsLookup, storage: Option<&Storage>) -> Cost {
    let matched_rows = estimate_rows(&PhysicalPlan::IndexScan(scan.clone()), stats);
    let height = index_height(storage, &scan.index_name);
    index_scan_cost(height, matched_rows)
}

fn index_nested_loop_join_node_cost(
    join: &IndexNestedLoopJoinNode,
    plan: &PhysicalPlan,
    stats: &dyn StatsLookup,
    storage: Option<&Storage>,
) -> Cost {
    let left_rows = estimate_rows(&join.left, stats);
    let height = index_height(storage, &join.index_name);
    let output_rows = estimate_rows(plan, stats);
    let avg_matches = if left_rows > 0 { output_rows as f64 / left_rows as f64 } else { 0.0 };
    plan_cost(&join.left, stats, storage) + index_nested_loop_join_cost(left_rows, height, avg_matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 単調性: 行数・ページ数が増えるとコストも増える ----

    #[test]
    fn seq_scan_cost_increases_with_more_pages() {
        let small = seq_scan_cost(10, 1000);
        let large = seq_scan_cost(100, 1000);
        assert!(large.value() > small.value(), "small={small:?} large={large:?}");
    }

    #[test]
    fn seq_scan_cost_increases_with_more_rows() {
        let small = seq_scan_cost(10, 100);
        let large = seq_scan_cost(10, 10_000);
        assert!(large.value() > small.value());
    }

    #[test]
    fn index_scan_cost_increases_with_more_matched_rows() {
        let few = index_scan_cost(3, 1);
        let many = index_scan_cost(3, 1000);
        assert!(many.value() > few.value());
    }

    #[test]
    fn index_scan_cost_increases_with_tree_height() {
        let shallow = index_scan_cost(1, 10);
        let deep = index_scan_cost(5, 10);
        assert!(deep.value() > shallow.value());
    }

    #[test]
    fn nested_loop_join_cost_grows_with_the_product_of_both_sides() {
        let small = nested_loop_join_cost(10, 10);
        let large = nested_loop_join_cost(100, 100);
        assert!(large.value() > small.value());
        // |L| x |R|であって|L|+|R|ではないことを、桁の増え方で確認する。
        // 10x10=100、100x100=10,000で、後者は前者のちょうど100倍になる。
        assert!((large.value() / small.value() - 100.0).abs() < 1e-6, "ratio={}", large.value() / small.value());
    }

    #[test]
    fn hash_join_cost_grows_with_the_sum_of_both_sides() {
        let small = hash_join_cost(10, 10);
        let large = hash_join_cost(1000, 1000);
        assert!(large.value() > small.value());
    }

    #[test]
    fn sort_cost_is_zero_for_zero_or_one_row() {
        assert_eq!(sort_cost(0), Cost::ZERO);
        assert_eq!(sort_cost(1), Cost::ZERO);
    }

    #[test]
    fn sort_cost_increases_with_more_rows() {
        let small = sort_cost(10);
        let large = sort_cost(10_000);
        assert!(large.value() > small.value());
    }

    // ---- Sequential ScanとRandom Access(Index Scan)の逆転 ----

    #[test]
    fn a_highly_selective_index_scan_is_cheaper_than_a_seq_scan() {
        // 100万行のテーブルから1行だけ一致するPoint述語。索引は
        // ごく浅い(高さ3)ので、IndexScanは数ページのRandom I/Oで済む。
        let seq = seq_scan_cost(20_000, 1_000_000);
        let index = index_scan_cost(3, 1);
        assert!(index.value() < seq.value(), "seq={seq:?} index={index:?}");
    }

    #[test]
    fn a_low_selectivity_index_scan_is_more_expensive_than_a_seq_scan() {
        // ほぼ全行が一致するとき、IndexScanは一致行数ぶんのRandom I/Oを
        // 1件ずつ払うのに対し、SeqScanはPage単位のSequential I/Oで済む。
        let seq = seq_scan_cost(20_000, 1_000_000);
        let index = index_scan_cost(3, 900_000);
        assert!(index.value() > seq.value(), "seq={seq:?} index={index:?}");
    }

    #[test]
    fn seq_scan_and_index_scan_costs_cross_over_as_selectivity_changes() {
        // 選択率を0から1へ動かすと、あるところでSeqScanとIndexScanの
        // 大小関係が入れ替わる境界が存在する。
        let total_rows = 1_000_000u64;
        let pages = 20_000u64;
        let seq = seq_scan_cost(pages, total_rows);
        let mut crossed = false;
        let mut previously_cheaper = None;
        for pct in 0..=100u64 {
            let matched = total_rows * pct / 100;
            let index = index_scan_cost(3, matched);
            let cheaper = index.value() < seq.value();
            if let Some(prev) = previously_cheaper
                && prev != cheaper
            {
                crossed = true;
            }
            previously_cheaper = Some(cheaper);
        }
        assert!(crossed, "選択率を動かしてもSeqScan/IndexScanの大小関係が一度も入れ替わらなかった");
    }

    // ---- IndexNestedLoopJoinとHashJoinの逆転(第25章の実測ケースの回収) ----

    /// `HashJoin`の総コスト(内側テーブルを1回全件読むSeqScan + Build/Probe)。
    /// `IndexNestedLoopJoin`は内側テーブルを全件読まない([`index_nested_loop_join_cost`]の
    /// ドキュメントを参照)ため、公平に比べるにはこのSeqScan分のI/Oコストを
    /// 加えて初めて、[`crate::physical_plan::optimize`]が実際に比較する
    /// `plan_cost`同士の大小関係に対応する。
    fn hash_join_total_cost(left_rows: u64, right_rows: u64) -> Cost {
        let right_pages = right_rows.div_ceil(DEFAULT_ROWS_PER_PAGE).max(1);
        seq_scan_cost(right_pages, right_rows) + hash_join_cost(left_rows, right_rows)
    }

    #[test]
    fn a_selective_join_favors_index_nested_loop_join() {
        // 50行の外側に対し、内側の一致がごく一部(平均0.66行/probe)にとどまる
        // 選択的な結合(第25章の`selective`実測と同じ規模)。
        let left_rows = 50;
        let right_rows = 32_000;
        let inlj = index_nested_loop_join_cost(left_rows, 3, 33.0 / left_rows as f64);
        let hash = hash_join_total_cost(left_rows, right_rows);
        assert!(inlj.value() < hash.value(), "inlj={inlj:?} hash={hash:?}");
    }

    #[test]
    fn a_dense_join_favors_hash_join() {
        // 内側のほぼ全行が一致する密な結合。第25章の実測(m=32000で
        // Index Nested Loop JoinがHash Joinの15倍以上遅い)と同じ場面。
        let left_rows = 50;
        let right_rows = 32_000;
        let avg_matches = right_rows as f64 / left_rows as f64; // ほぼ全行が一致
        let inlj = index_nested_loop_join_cost(left_rows, 3, avg_matches);
        let hash = hash_join_total_cost(left_rows, right_rows);
        assert!(hash.value() < inlj.value(), "inlj={inlj:?} hash={hash:?}");
    }

    #[test]
    fn increasing_average_matches_can_flip_the_join_choice() {
        // 一致件数(密度)を増やしていくと、Index Nested Loop JoinとHash Join
        // の大小関係が入れ替わる境界が存在する。
        let left_rows = 50;
        let right_rows = 32_000;
        let hash = hash_join_total_cost(left_rows, right_rows);
        let mut crossed = false;
        let mut previously_cheaper = None;
        for matches in [0.001, 1.0, 10.0, 100.0, 640.0] {
            let inlj = index_nested_loop_join_cost(left_rows, 3, matches);
            let cheaper = inlj.value() < hash.value();
            if let Some(prev) = previously_cheaper
                && prev != cheaper
            {
                crossed = true;
            }
            previously_cheaper = Some(cheaper);
        }
        assert!(crossed, "一致件数を動かしてもIndex Nested Loop Join/Hash Joinの大小関係が一度も入れ替わらなかった");
    }

    // ---- 統計・実データの無いテーブルに対するフォールバック ----

    #[test]
    fn table_pages_falls_back_to_rows_divided_by_default_rows_per_page_without_storage() {
        // `storage`が`None`(`Backend::Memory`)のときは、ページ数を
        // `rows / DEFAULT_ROWS_PER_PAGE`から逆算する。
        let pages = table_pages(None, TableId(1), DEFAULT_ROWS_PER_PAGE * 10);
        assert_eq!(pages, 10);
    }

    #[test]
    fn table_pages_never_returns_zero_even_for_a_tiny_row_count() {
        let pages = table_pages(None, TableId(1), 1);
        assert!(pages >= 1);
    }

    #[test]
    fn index_height_falls_back_to_the_default_without_storage() {
        assert_eq!(index_height(None, "no_such_index"), DEFAULT_INDEX_HEIGHT);
    }
}
