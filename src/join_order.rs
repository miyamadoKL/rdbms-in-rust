//! 3個以上のテーブルを結合する`FROM`に対する、Join Orderの探索(第29章)。
//!
//! 第28章までの[`crate::physical_plan::optimize`]は、`FROM`に書かれた`JOIN`の
//! 並びをそのまま左深い木として組み立て、個々のJoinの実行アルゴリズム
//! (`HashJoin`・`IndexNestedLoopJoin`)だけをコストで選んでいた。`JOIN`が2個
//! 以上(テーブルが3個以上)になると、その並び自体にも選択の余地が生まれる。
//! `A JOIN B JOIN C`という同じ結合対象の集合でも、`(A JOIN B) JOIN C`と
//! `(A JOIN C) JOIN B`とでは、途中でできる中間結果の行数が異なり、それが
//! そのまま次のJoinのコストに響く。この章がやることは、SQLに書いた構文順を
//! 1つの候補として扱いながら、他の並びとコストで比較し、最小のものを選ぶことである。
//!
//! # 動的計画法(Selinger型)とLeft-deep限定
//!
//! `n`個のテーブルを結合する順列は`n!`通りあり、さらに木の形(どちらを
//! `left`にしてどちらを`right`にするか)まで数えると候補はもっと多い。
//! この章は、System Rが1979年の論文で示した2つの制限を踏襲する。
//!
//! 1. **Left-deep木に限定する**。`right`は必ず単一のテーブル(の物理計画)で、
//!    それまでに結合したテーブル群を表す`left`に1個ずつ追加していく形しか
//!    考えない。`(A JOIN B) JOIN (C JOIN D)`のような、両側が複数テーブルの
//!    木(Bushy木)は候補にしない。
//! 2. **部分集合ごとに最良の1個だけを覚える**。「テーブル`{A, B, C}`を
//!    結合する`left`側の作り方」は、内部の結合順序が何通りあっても、
//!    コスト最小の1個だけを覚えておけば十分である。`{A, B, C, D}`を作るとき
//!    にどの順序で`{A, B, C}`を作ったかは、それ単体のコストが分かっていれば
//!    もう関係ない(最適性の原理)。
//!
//! これにより、候補は「テーブルの部分集合」の数(`2^n`)だけに収まる。本文の
//! 実装は`u32`のビットマスクで部分集合を表す。
//!
//! # Cartesian Productの抑制
//!
//! `ON`条件を1つも持たない2つのテーブル同士(構文上は`CROSS JOIN`に近い)を
//! 結合すると、結果行数は双方の行数の積になり、たいてい爆発する。この章の
//! DPは、部分集合を1個のテーブルで拡張するとき、その新しいテーブルが
//! すでに含まれるどれかのテーブルと`ON`条件で繋がっている(**Connected**な)
//! 拡張を優先する。繋がる拡張が1つも無い場合に限り、繋がらない拡張
//! (Cartesian Product)を許す。
//!
//! # Physical Properties: Interesting Orderの扱い
//!
//! 原案(`docs-local/chatgpt_opinion.md`)はPhysical Propertyを主に「出力順序」
//! として扱うとしている。この章もそれを踏襲するが、DPの状態そのものに
//! 順序を持ち込むことはしない。教科書的なInteresting Order DPは、部分集合
//! ごとに「コスト最小の1個」ではなく「(コスト, 出力順序)の組ごとに
//! Pareto最適な複数個」を覚え、それぞれの順序が後続の`Sort`を省く形で
//! 得になるかを比較する。この章のDPは部分集合ごとに1個の計画しか残さない
//! 単純化のままにし、出力順序の活用は
//! [`crate::physical_plan::output_ordering`]による**Sortの省略**
//! (`crate::physical_plan::optimize`の`LogicalPlan::Sort`の分岐)だけに絞る。
//! DPの中まで順序を持ち込まない理由は、この教材の規模(テーブル数は高々
//! [`MAX_DP_TABLES`])では、Join順序を1段階変えるだけで得られるコスト差
//! (本文の実測)の方が、順序を保つために準最適なJoin順序を選ぶことで
//! 得られるであろうSort省略の効果よりずっと大きく、状態を(コスト,順序)の
//! 組へ増やす実装の複雑さに見合わないと判断したためである(発展課題)。

use std::collections::HashMap;

use crate::ast::{BinaryOperator, JoinKind};
use crate::binder::{BoundExpr, BoundSelectItem};
use crate::cost_model::{self, Cost};
use crate::lexer::Span;
use crate::logical_plan::{self, LogicalPlan};
use crate::physical_plan::{self, FilterNode, NestedLoopJoinNode, PhysicalPlan, ProjectionNode, StatsLookup};
use crate::storage::Storage;
use crate::types::{DataType, Schema};

/// DPが状態(部分集合)を`u32`のビットマスクで表せる上限のテーブル数。
///
/// DPの状態数は`2^n`である。`n = 8`なら256状態、各状態が高々8個の拡張先を
/// 試すため計算量は2,000通り程度に収まり、教材の実行時間として問題にならない。
/// `n`がこれを超える`FROM`(3〜4テーブルを主な対象とするこの章の演習の範囲を
/// 大きく超える)は、[`optimize_join_order`]がDPを打ち切り、構文順の左深い木
/// (第22〜28章までと同じ組み立て方)へフォールバックする。テーブル数がさらに
/// 増えても組み合わせ爆発せずに済む一方、そのフォールバックが選ぶ順序が
/// 最適とは限らない、という制約はこの章の範囲では受け入れる(章末の演習課題)。
pub const MAX_DP_TABLES: usize = 8;

/// DPの1状態(部分集合)が持つ、その部分集合に対する最良の計画。
#[derive(Clone)]
struct DpEntry {
    /// この部分集合を結合した左深いPhysicalPlan。
    plan: PhysicalPlan,
    /// `plan`を組み立てるのに使ったテーブルの並び(元の`FROM`での位置、
    /// 0始まり)。`plan`の出力スキーマは、この並びの各テーブルのスキーマを
    /// 順に連結したものになる。
    order: Vec<usize>,
    cost: Cost,
}

/// [`crate::physical_plan::optimize`]の`LogicalPlan::Join`が、葉が3個以上の
/// 連鎖を見つけたときに呼ぶ入口。`leaves[i]`(`Scan`または`Filter(Scan)`)と
/// `conditions[i-1]`(`leaves[i]`を結合する`ON`条件、元の`FROM`順)が対応する
/// (`crate::physical_plan::flatten_join_chain`が作る形)。
///
/// `conditions`の`BoundExpr::ColumnRef`は、`leaves`をすべて元の順序で連結した
/// **結合後スキーマ**上のフラットな添字を持つ(`Binder`が束縛の時点で決めた
/// もので、`Join`の構造とは無関係に固定されている)。この章のDPは、探索した
/// 左深い順序ごとに列の並びが変わるため、条件をその都度その順序向けの添字へ
/// 組み替える(`remap_condition`)。最終的に選んだ順序が元の`FROM`順と異なる
/// 場合は、呼び出し元(`Filter`・`Projection`等、この`Join`の上に乗る演算子)が
/// 引き続き元の結合後スキーマの添字を使えるよう、列を元の並びへ戻す
/// `Projection`を1段だけ追加する(`reorder_to_original_layout`)。
pub fn optimize_join_order(
    leaves: Vec<LogicalPlan>,
    conditions: Vec<BoundExpr>,
    storage: Option<&Storage>,
    stats: &dyn StatsLookup,
) -> PhysicalPlan {
    let n = leaves.len();
    debug_assert_eq!(conditions.len(), n - 1, "n個の葉に対してONは常にn-1個");

    if n > MAX_DP_TABLES {
        let leaf_plans: Vec<PhysicalPlan> = leaves.into_iter().map(|leaf| physical_plan::optimize(leaf, storage, stats)).collect();
        return combine_in_syntactic_order(leaf_plans, conditions, storage, stats);
    }

    let widths: Vec<usize> = leaves.iter().map(|leaf| leaf.output_schema().len()).collect();
    let orig_offset = prefix_sums(&widths);
    let original_schema = concat_schema(&leaves);
    // DPが選んだ順序が構文順と異なる場合、列を元の結合後スキーマの並びへ
    // 戻す`Projection`を1段追加する必要がある(`reorder_to_original_layout`)。
    // その`Projection`自体、結果行数ぶんの`CPU_TUPLE_COST`という無視できない
    // コストを持つため、DPの内部コストだけで「安い」と判断してよいとは限らない。
    // 構文順(常にこの並べ替えが不要)の計画をここで別途組み立てておき、
    // 最後にDPの計画(並べ替え込み)と実際に比較して、構文順のほうが総コストで
    // 勝るならそちらを採用する(`leaves`・`conditions`を分解する前のもの
    // (`raw_leaves`・`conditions`そのもの)を使い、`extra_leaf_filters`による
    // 二重適用を避ける)。
    let raw_leaves = leaves.clone();

    // 各`ON`条件をANDの連言に分解し、参照するテーブルの数で仕分ける。
    // ちょうど2個のテーブルを参照する項が、Join Graphの辺になる。1個だけを
    // 参照する項はその葉自身のFilterへ、0個または3個以上を参照する項は
    // 最後にまとめて`Filter`で結果全体へ適用する(`residual`)。
    let mut edges: HashMap<(usize, usize), Vec<BoundExpr>> = HashMap::new();
    let mut extra_leaf_filters: Vec<Vec<BoundExpr>> = vec![Vec::new(); n];
    let mut residual: Vec<BoundExpr> = Vec::new();
    for condition in &conditions {
        let mut conjuncts = Vec::new();
        physical_plan::collect_conjuncts(condition, &mut conjuncts);
        for conjunct in conjuncts {
            let mut tables = Vec::new();
            referenced_tables(conjunct, &mut tables);
            match tables.as_slice() {
                [] => residual.push(conjunct.clone()),
                [only] => {
                    let local = remap_condition(conjunct, &orig_offset, &HashMap::from([(*only, 0)]));
                    extra_leaf_filters[*only].push(local);
                }
                [a, b] => {
                    let (a, b) = (*a, *b);
                    edges.entry((a.min(b), a.max(b))).or_default().push(conjunct.clone());
                }
                _ => residual.push(conjunct.clone()),
            }
        }
    }

    let leaf_plans: Vec<PhysicalPlan> = leaves
        .into_iter()
        .zip(extra_leaf_filters)
        .map(|(leaf, extra)| {
            let leaf = match physical_plan::rebuild_conjunction(extra) {
                Some(predicate) => merge_leaf_filter(leaf, predicate),
                None => leaf,
            };
            physical_plan::optimize(leaf, storage, stats)
        })
        .collect();

    // 単一テーブルの部分集合(ビット1個)を、DPの初期状態として登録する。
    let mut dp: HashMap<u32, DpEntry> = HashMap::new();
    for (i, plan) in leaf_plans.iter().enumerate() {
        let cost = cost_model::plan_cost(plan, stats, storage);
        dp.insert(1u32 << i, DpEntry { plan: plan.clone(), order: vec![i], cost });
    }

    // `mask`を値の昇順に見ていけば、`mask`からどれか1ビット落とした
    // `prev_mask`は必ず`mask`より小さい値になるため、`prev_mask`のDPは
    // この時点ですでに確定している。
    let full_mask: u32 = (1u32 << n) - 1;
    for mask in 1u32..=full_mask {
        if mask.count_ones() < 2 {
            continue; // ビット1個の部分集合は初期状態としてすでに登録済み
        }
        let mut connected_best: Option<DpEntry> = None;
        let mut disconnected_best: Option<DpEntry> = None;
        for (i, right_plan) in leaf_plans.iter().enumerate() {
            let bit = 1u32 << i;
            if mask & bit == 0 {
                continue;
            }
            let prev_mask = mask & !bit;
            let Some(prev) = dp.get(&prev_mask) else { continue };

            let mut new_order = prev.order.clone();
            new_order.push(i);
            let new_offset = offsets_for_order(&new_order, &widths);
            let condition = connecting_condition(&edges, prev_mask, i, n, &orig_offset, &new_offset);
            let is_connected = condition.is_some();

            let left = prev.plan.clone();
            let right = right_plan.clone();
            let plan = join_leaf(left, right, condition, storage, stats);
            let cost = cost_model::plan_cost(&plan, stats, storage);
            let candidate = DpEntry { plan, order: new_order, cost };

            let slot = if is_connected { &mut connected_best } else { &mut disconnected_best };
            if slot.as_ref().is_none_or(|best: &DpEntry| candidate.cost.value() < best.cost.value()) {
                *slot = Some(candidate);
            }
        }
        // 結合条件で繋がる拡張があればそれを優先し、1つも無いとき(この
        // 部分集合がどのテーブルとも`ON`で繋がっていないとき)に限り
        // Cartesian Productを許す(モジュール冒頭の説明を参照)。
        let best = connected_best.or(disconnected_best).expect("popcount>=2のmaskは、必ずどれかの葉を1個足す経路を持つ");
        dp.insert(mask, best);
    }

    let best = dp.remove(&full_mask).expect("full_maskは必ず埋まる(全テーブルがいずれかの経路で結合される)");
    let reordered = reorder_to_original_layout(best.plan, &best.order, &widths, &original_schema);

    let syntactic_leaf_plans: Vec<PhysicalPlan> =
        raw_leaves.into_iter().map(|leaf| physical_plan::optimize(leaf, storage, stats)).collect();
    let syntactic = combine_in_syntactic_order(syntactic_leaf_plans, conditions, storage, stats);

    let plan = if cost_model::plan_cost(&reordered, stats, storage).value() < cost_model::plan_cost(&syntactic, stats, storage).value() {
        reordered
    } else {
        syntactic
    };
    wrap_residual(plan, residual)
}

/// `left`(それまでに結合した部分集合)と`right`(新しく加える1個の葉)を、
/// `condition`(2つを繋ぐ`ON`条件。無ければCartesian Product)で結合する。
/// [`crate::physical_plan::choose_join_plan`]・`split_equi_join_keys`という
/// 第22・25・28章の判断(等値条件が取り出せればHash JoinとIndex Nested Loop
/// Joinをコストで比較し、取り出せなければNested Loop Join)をそのまま再利用する。
fn join_leaf(
    left: PhysicalPlan,
    right: PhysicalPlan,
    condition: Option<BoundExpr>,
    storage: Option<&Storage>,
    stats: &dyn StatsLookup,
) -> PhysicalPlan {
    let left_len = left.output_schema().len();
    let condition = condition.unwrap_or_else(|| BoundExpr::BoolLiteral { value: true, span: Span::new(0, 0) });
    match physical_plan::split_equi_join_keys(&condition, left_len) {
        Some(keys) => {
            let keys: Vec<(BoundExpr, BoundExpr)> = keys
                .into_iter()
                .map(|(left_key, right_key)| (left_key, physical_plan::shift_column_index(&right_key, left_len)))
                .collect();
            physical_plan::choose_join_plan(storage, stats, left, right, JoinKind::Inner, condition, keys)
        }
        None => PhysicalPlan::NestedLoopJoin(NestedLoopJoinNode {
            left: Box::new(left),
            right: Box::new(right),
            kind: JoinKind::Inner,
            condition,
        }),
    }
}

/// テーブル数が[`MAX_DP_TABLES`]を超えたときのフォールバック。DPを一切行わず、
/// 元の`FROM`に書かれた順序のまま左深い木を組み立てる(第22〜28章までの
/// 挙動そのもの)。`conditions`は元の結合後スキーマの添字のままでよい
/// (構文順のまま`left`へ1個ずつ足していく限り、列の並びは元の`FROM`順と
/// 一致し続けるため、`optimize_join_order`のDP経路のような組み替えが不要)。
pub(crate) fn combine_in_syntactic_order(
    leaf_plans: Vec<PhysicalPlan>,
    conditions: Vec<BoundExpr>,
    storage: Option<&Storage>,
    stats: &dyn StatsLookup,
) -> PhysicalPlan {
    let mut leaves = leaf_plans.into_iter();
    let mut plan = leaves.next().expect("葉は1個以上");
    for (right, condition) in leaves.zip(conditions) {
        plan = join_leaf(plan, right, Some(condition), storage, stats);
    }
    plan
}

/// `prev_mask`(すでに結合済みのテーブル)と`i`(新しく加えるテーブル)を繋ぐ
/// `ON`条件を、`new_offset`(`prev_mask`のテーブル + `i`を並べた新しい配置での
/// 列オフセット)に合わせて組み立てる。`prev_mask`内の複数のテーブルが`i`と
/// それぞれ別の辺で繋がっている場合は、それらすべてをANDで連結する
/// (`i`が複数のテーブルの外部キーを持つ場合など)。1本も見つからなければ
/// `None`(Cartesian Productでしか`i`を足せないことを表す)。
fn connecting_condition(
    edges: &HashMap<(usize, usize), Vec<BoundExpr>>,
    prev_mask: u32,
    i: usize,
    n: usize,
    orig_offset: &[usize],
    new_offset: &HashMap<usize, usize>,
) -> Option<BoundExpr> {
    let mut conjuncts = Vec::new();
    for j in 0..n {
        if prev_mask & (1u32 << j) == 0 {
            continue;
        }
        let key = (i.min(j), i.max(j));
        if let Some(list) = edges.get(&key) {
            conjuncts.extend(list.iter().map(|c| remap_condition(c, orig_offset, new_offset)));
        }
    }
    physical_plan::rebuild_conjunction(conjuncts)
}

/// `expr`の中の`ColumnRef`を、`table_ordinal`ごとの元のオフセット(`orig_offset`、
/// 元の`FROM`順で連結した結合後スキーマ)から一度ローカルな添字へ戻し、
/// `new_offset`(移し先の配置でのテーブルごとのオフセット)へ再配置する。
/// `crate::physical_plan::shift_column_index`と同じ形の再帰だが、あちらが
/// 全体を同じ量だけ動かす(定数シフト)のに対し、こちらはテーブルごとに
/// 異なる移動量を`new_offset`から引く点が違う(だけ)である。
fn remap_condition(expr: &BoundExpr, orig_offset: &[usize], new_offset: &HashMap<usize, usize>) -> BoundExpr {
    match expr {
        BoundExpr::ColumnRef { table_ordinal, column_index, name, data_type, span } => {
            let local = column_index - orig_offset[*table_ordinal];
            let new_index = new_offset[table_ordinal] + local;
            BoundExpr::ColumnRef {
                table_ordinal: *table_ordinal,
                column_index: new_index,
                name: name.clone(),
                data_type: *data_type,
                span: *span,
            }
        }
        BoundExpr::UnaryOp { op, expr, data_type, span } => {
            BoundExpr::UnaryOp { op: *op, expr: Box::new(remap_condition(expr, orig_offset, new_offset)), data_type: *data_type, span: *span }
        }
        BoundExpr::BinaryOp { op, lhs, rhs, data_type, span } => BoundExpr::BinaryOp {
            op: *op,
            lhs: Box::new(remap_condition(lhs, orig_offset, new_offset)),
            rhs: Box::new(remap_condition(rhs, orig_offset, new_offset)),
            data_type: *data_type,
            span: *span,
        },
        BoundExpr::IsNull { expr, negated, span } => {
            BoundExpr::IsNull { expr: Box::new(remap_condition(expr, orig_offset, new_offset)), negated: *negated, span: *span }
        }
        BoundExpr::FunctionCall { name, args, data_type, span } => BoundExpr::FunctionCall {
            name: name.clone(),
            args: args.iter().map(|arg| remap_condition(arg, orig_offset, new_offset)).collect(),
            data_type: *data_type,
            span: *span,
        },
        BoundExpr::Paren { expr, span } => BoundExpr::Paren { expr: Box::new(remap_condition(expr, orig_offset, new_offset)), span: *span },
        BoundExpr::Cast { expr, data_type, span } => {
            BoundExpr::Cast { expr: Box::new(remap_condition(expr, orig_offset, new_offset)), data_type: *data_type, span: *span }
        }
        // 列参照を持たない項(リテラル)、およびON句に現れない`Aggregate`は
        // そのまま複製する。
        other => other.clone(),
    }
}

/// `expr`が参照するテーブル(`table_ordinal`)の集合を、出現順の重複無しで集める。
fn referenced_tables(expr: &BoundExpr, out: &mut Vec<usize>) {
    match expr {
        BoundExpr::ColumnRef { table_ordinal, .. } => {
            if !out.contains(table_ordinal) {
                out.push(*table_ordinal);
            }
        }
        BoundExpr::UnaryOp { expr, .. } | BoundExpr::Paren { expr, .. } | BoundExpr::Cast { expr, .. } | BoundExpr::IsNull { expr, .. } => {
            referenced_tables(expr, out)
        }
        BoundExpr::BinaryOp { lhs, rhs, .. } => {
            referenced_tables(lhs, out);
            referenced_tables(rhs, out);
        }
        BoundExpr::FunctionCall { args, .. } => {
            for arg in args {
                referenced_tables(arg, out);
            }
        }
        BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. }
        | BoundExpr::Aggregate { .. } => {}
    }
}

/// `leaf`(`Scan`または既存の`Filter(Scan)`)の直上に、`predicate`(すでに
/// `leaf`自身のローカルな添字に組み替え済み)をANDで合成した`Filter`を作る。
/// 第26章のPredicate Pushdownがすでに`Filter(Scan)`を作っていれば、その
/// `predicate`と合成する(2つの`Filter`を積み重ねるのではなく1つにまとめる。
/// `choose_scan_plan`は`Filter`直下が`Scan`であることを前提にしているため)。
fn merge_leaf_filter(leaf: LogicalPlan, predicate: BoundExpr) -> LogicalPlan {
    match leaf {
        LogicalPlan::Filter(filter) => {
            let span = filter.predicate.span();
            let combined = BoundExpr::BinaryOp {
                op: BinaryOperator::And,
                lhs: Box::new(filter.predicate),
                rhs: Box::new(predicate),
                data_type: DataType::Boolean,
                span,
            };
            LogicalPlan::Filter(logical_plan::FilterNode { input: filter.input, predicate: combined })
        }
        other => LogicalPlan::Filter(logical_plan::FilterNode { input: Box::new(other), predicate }),
    }
}

/// `residual`(0個または3個以上のテーブルを参照する`ON`条件の項。この章の
/// Join Graphが辺として扱わない項)が1個以上あれば、`plan`の上に`Filter`として
/// 適用する。`residual`が空ならそのまま`plan`を返す。
fn wrap_residual(plan: PhysicalPlan, residual: Vec<BoundExpr>) -> PhysicalPlan {
    match physical_plan::rebuild_conjunction(residual) {
        Some(predicate) => PhysicalPlan::Filter(FilterNode { input: Box::new(plan), predicate }),
        None => plan,
    }
}

/// DPが選んだ`plan`(`order`という並びでテーブルを連結した結果、列が元の
/// `FROM`順とは限らない配置になっている)の列を、元の結合後スキーマの並びへ
/// 戻す。`order`がすでに元の順序(`0, 1, 2, ...`)と一致するなら並べ替えは
/// 不要なので、`Projection`を追加せずそのまま返す。
///
/// この並べ替えが要るのは、`plan`の親(この`Join`の上に乗る`Filter`・
/// `Projection`・`Aggregate`等)が、`Binder`が結合後スキーマ全体に対して
/// 割り当てた`column_index`をそのまま使い続けるためである。DPが構文順とは
/// 違う順序を選んでも、それより上の演算子から見える列の並びは変わらない
/// ようにする必要がある。
fn reorder_to_original_layout(plan: PhysicalPlan, order: &[usize], widths: &[usize], original_schema: &Schema) -> PhysicalPlan {
    let n = widths.len();
    if order.iter().copied().eq(0..n) {
        return plan;
    }

    let orig_offset = prefix_sums(widths);
    let new_offset = offsets_for_order(order, widths);
    let projection: Vec<BoundSelectItem> = original_schema
        .columns()
        .iter()
        .enumerate()
        .map(|(global_index, column)| {
            let table_ordinal = owning_table(global_index, &orig_offset, widths);
            let local = global_index - orig_offset[table_ordinal];
            let new_index = new_offset[&table_ordinal] + local;
            BoundSelectItem {
                expr: BoundExpr::ColumnRef {
                    table_ordinal,
                    column_index: new_index,
                    name: column.name.clone(),
                    data_type: column.data_type,
                    span: Span::new(0, 0),
                },
                output_name: column.name.clone(),
            }
        })
        .collect();
    PhysicalPlan::Projection(ProjectionNode { input: Box::new(plan), projection })
}

/// `widths`(各テーブルの列幅)から、元の`FROM`順でテーブルごとの結合後
/// スキーマ上のオフセットを求める(`table_ordinal`が`t`のテーブルの先頭列は
/// 添字`sums[t]`から始まる)。
fn prefix_sums(widths: &[usize]) -> Vec<usize> {
    let mut sums = Vec::with_capacity(widths.len());
    let mut acc = 0;
    for &width in widths {
        sums.push(acc);
        acc += width;
    }
    sums
}

/// `order`(テーブルの並び)に沿って`widths`を連結したときの、テーブルごとの
/// オフセット。
fn offsets_for_order(order: &[usize], widths: &[usize]) -> HashMap<usize, usize> {
    let mut map = HashMap::with_capacity(order.len());
    let mut offset = 0;
    for &table_ordinal in order {
        map.insert(table_ordinal, offset);
        offset += widths[table_ordinal];
    }
    map
}

/// `global_index`(元の結合後スキーマ上の添字)が、元の`FROM`順でどの
/// テーブル(`table_ordinal`)に属するかを求める。
fn owning_table(global_index: usize, orig_offset: &[usize], widths: &[usize]) -> usize {
    for (table_ordinal, &offset) in orig_offset.iter().enumerate() {
        if global_index >= offset && global_index < offset + widths[table_ordinal] {
            return table_ordinal;
        }
    }
    unreachable!("global_indexは結合後スキーマの列数未満のはず")
}

fn concat_schema(leaves: &[LogicalPlan]) -> Schema {
    let mut columns = Vec::new();
    for leaf in leaves {
        columns.extend(leaf.output_schema().columns().iter().cloned());
    }
    Schema::new(columns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{Binder, BoundStatement};
    use crate::catalog::Catalog;
    use crate::estimator::DEFAULT_ROW_COUNT_ESTIMATE;
    use crate::logical_plan::build_select;
    use crate::parser::parse_statement;
    use crate::physical_plan::NoStats;
    use crate::statistics::{ColumnStats, TableStats};
    use crate::types::{Column, DataType};

    /// `A`(小)・`B`(ハブ、大)・`C`(大)の3テーブルからなるチェーン(`A - B - C`)。
    /// `A`・`C`の間には`ON`条件が無い(直接の辺は無い)。`A - B`は主キー・
    /// 外部キー型の選択的な結合(`b.a_id = a.id`)、`B - C`は共有する低NDV列
    /// (`country`)どうしを突き合わせる選択的でない結合(`b.country = c.country`)
    /// にしてある。低NDV列どうしの結合は「双方の値がどちらも粗い」ときに
    /// 初めて中間結果が膨らむ(`estimate_join_row_count`の`max(NDV)`が小さい
    /// ままになる)。この`B - C`を先に結合すると、選択的な`A`を後回しにする
    /// せいで大きな中間結果を長く抱えることになる(本文の実測)。
    fn abc_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                "a",
                Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("v", DataType::BigInt, true)]),
            )
            .unwrap();
        catalog
            .create_table(
                "b",
                Schema::new(vec![
                    Column::new("id", DataType::BigInt, false),
                    Column::new("a_id", DataType::BigInt, true),
                    Column::new("country", DataType::BigInt, true),
                ]),
            )
            .unwrap();
        catalog
            .create_table(
                "c",
                Schema::new(vec![
                    Column::new("id", DataType::BigInt, false),
                    Column::new("country", DataType::BigInt, true),
                ]),
            )
            .unwrap();
        catalog
    }

    /// `table_id`(`Catalog::create_table`順で0始まり)ごとの行数・NDVだけを
    /// 持つ、テスト専用の`StatsLookup`。Histogramは持たない(等値・結合の
    /// 選択率がNDVだけから決まるようにして、テストの数字を追いやすくする)。
    struct FixedStats {
        tables: Vec<TableStats>,
    }

    impl StatsLookup for FixedStats {
        fn table_stats(&self, table_id: crate::ids::TableId) -> Option<&TableStats> {
            self.tables.get(table_id.0 as usize)
        }
    }

    fn column_stats(distinct_count: u64) -> ColumnStats {
        ColumnStats { distinct_count, null_count: 0, min: None, max: None, histogram: Vec::new() }
    }

    /// `A`(5行)・`B`(5000行)・`C`(5000行)の統計。`b.a_id`のNDV(1000)は
    /// `a.id`のNDV(5)よりずっと大きく、`A - B`の結合は選択的(見積もり25行)に
    /// なる。`b.country`・`c.country`はどちらもNDV10の低カーディナリティ列で、
    /// `B - C`の結合は見積もり250万行まで膨らむ(`abc_catalog`のドキュメント
    /// 参照)。
    fn abc_stats() -> FixedStats {
        FixedStats {
            tables: vec![
                TableStats { row_count: 5, columns: vec![column_stats(5), column_stats(5)] }, // a
                TableStats { row_count: 5000, columns: vec![column_stats(5000), column_stats(1000), column_stats(10)] }, // b
                TableStats { row_count: 5000, columns: vec![column_stats(5000), column_stats(10)] }, // c
            ],
        }
    }

    fn bind_select(sql: &str, catalog: &Catalog) -> crate::binder::BoundSelect {
        let functions = crate::eval::FunctionRegistry::with_builtins();
        let statement = parse_statement(sql).unwrap();
        match Binder::new(catalog, &functions, sql).bind(statement).unwrap() {
            BoundStatement::Select(select) => *select,
            other => panic!("Selectを期待したが{other:?}が返った"),
        }
    }

    fn optimize_sql(sql: &str, catalog: &Catalog, stats: &dyn StatsLookup) -> PhysicalPlan {
        let select = bind_select(sql, catalog);
        // 第26章のルールベース最適化(特にProjection Pruning)は経由しない。
        // DPの振る舞いをそれ単体で検証したいテストであり、Pruningが列を
        // 間引くと`resolve_column_owner`がProjectionの先を追わずNDVを
        // 見失う(第27章の限界)ため、この章の実測とは無関係な変動要因になる。
        physical_plan::optimize(build_select(select), None, stats)
    }

    /// `optimize_sql`と違い、`join_order`のDPを経由せず、`sql`の`FROM`に
    /// 書かれた順序のまま左深い木を組み立てる(`combine_in_syntactic_order`を
    /// 直接呼ぶ)。DPが本当に構文順より安い計画を見つけているかどうかを、
    /// 「同じ`FROM`をDP有り/無しで比較する」形で検証するために使う。
    fn optimize_sql_in_syntactic_order(sql: &str, catalog: &Catalog, stats: &dyn StatsLookup) -> PhysicalPlan {
        let select = bind_select(sql, catalog);
        let logical = build_select(select);
        // `SELECT`が積む`Projection`の下から`Join`の根を取り出し、
        // `flatten_join_chain`(第29章、`optimize_join_order`と同じもの)で
        // 葉と`ON`条件へ平らにしてから、DPを経由しない`combine_in_syntactic_order`
        // で結合する。
        let join_root = find_join_root(logical);
        let mut leaves = Vec::new();
        let mut conditions = Vec::new();
        physical_plan::flatten_join_chain(join_root, &mut leaves, &mut conditions);
        let leaf_plans: Vec<PhysicalPlan> = leaves.into_iter().map(|leaf| physical_plan::optimize(leaf, None, stats)).collect();
        let joined = combine_in_syntactic_order(leaf_plans, conditions, None, stats);
        // テスト対象の`SELECT`は単純な列参照の`Projection`だけなので、
        // その最終`Projection`を`joined`の上に付け直す(`optimize_sql`が
        // 返す木の形と揃えるため。等価な比較にはこの1段だけで十分)。`v`は
        // 3テーブル(`a`・`b`・`c`)の中で`a`にしか無い列名なので、
        // 出力スキーマから位置を探せば`table_ordinal`を意識せず特定できる。
        let joined_schema = joined.output_schema();
        let column_index = joined_schema.columns().iter().position(|c| c.name == "v").expect("vはaにしか無い列");
        PhysicalPlan::Projection(physical_plan::ProjectionNode {
            input: Box::new(joined),
            projection: vec![BoundSelectItem {
                expr: BoundExpr::ColumnRef {
                    table_ordinal: 0,
                    column_index,
                    name: "v".to_string(),
                    data_type: crate::types::DataType::BigInt,
                    span: crate::lexer::Span::new(0, 0),
                },
                output_name: "v".to_string(),
            }],
        })
    }

    fn find_join_root(plan: LogicalPlan) -> LogicalPlan {
        match plan {
            LogicalPlan::Join(_) => plan,
            LogicalPlan::Projection(p) => find_join_root(*p.input),
            LogicalPlan::Filter(f) => find_join_root(*f.input),
            other => panic!("Joinの根が見つからない: {other:?}"),
        }
    }

    #[test]
    fn dp_prefers_joining_the_two_small_tables_before_the_large_one() {
        // 構文順は「Cを先に、Aを最後に」結合する悪い順序(低NDVどうしの
        // 結合を先に行い、250万行規模の中間結果を抱える)。DPは
        // 「AとBを先に(選択的、中間結果25行)、Cを最後に」結合する順序へ
        // 組み替えるはずである。
        let catalog = abc_catalog();
        let stats = abc_stats();
        let sql = "SELECT a.v FROM c JOIN b ON c.country = b.country JOIN a ON b.a_id = a.id";
        let syntactic = optimize_sql_in_syntactic_order(sql, &catalog, &stats);
        let chosen = optimize_sql(sql, &catalog, &stats);

        let syntactic_cost = cost_model::plan_cost(&syntactic, &stats, None).value();
        let chosen_cost = cost_model::plan_cost(&chosen, &stats, None).value();
        assert!(
            chosen_cost < syntactic_cost,
            "DPが選んだ計画({chosen_cost})は構文順の計画({syntactic_cost})より安いはず\nsyntactic={syntactic}\nchosen={chosen}"
        );

        // AとBが先に(選択的に)結合され、Cは最後に(木の一番外側で)結合されて
        // いるはず。`chosen`は列を元の並びへ戻す`Projection`を2段(この
        // クエリ自身の`SELECT`用と、第29章の並べ替え用)経由するので、
        // その下のJoinの根まで辿る。
        let PhysicalPlan::Projection(outer) = &chosen else { panic!("Projectionのはず: {chosen}") };
        let PhysicalPlan::Projection(reorder) = outer.input.as_ref() else { panic!("Projectionのはず: {chosen}") };
        let PhysicalPlan::HashJoin(root) = reorder.input.as_ref() else { panic!("HashJoinのはず: {chosen}") };
        let right_rows = physical_plan::estimate_rows(&root.right, &stats);
        assert_eq!(right_rows, 5000, "Cは最後に(rightとして)結合されるはず: {chosen}");
    }

    #[test]
    fn dp_and_syntactic_order_return_the_same_rows() {
        // 順序を変えても最終的な行集合(内容)は変わらない、という等価性。
        let mut db = crate::database::Database::memory();
        for stmt in [
            "CREATE TABLE a (id BIGINT NOT NULL, v BIGINT)",
            "CREATE TABLE b (id BIGINT NOT NULL, a_id BIGINT)",
            "CREATE TABLE c (id BIGINT NOT NULL, b_id BIGINT)",
            "INSERT INTO a VALUES (1, 100), (2, 200), (3, 300)",
            "INSERT INTO b VALUES (1, 1), (2, 2), (3, 3), (4, 1)",
            "INSERT INTO c VALUES (1, 1), (2, 2), (3, 4), (4, 3)",
        ] {
            db.execute(stmt).unwrap();
        }

        let dp_order = db
            .execute("SELECT a.v, b.id, c.id FROM c JOIN b ON c.b_id = b.id JOIN a ON b.a_id = a.id ORDER BY a.v, b.id, c.id")
            .unwrap();
        let syntactic_order = db
            .execute("SELECT a.v, b.id, c.id FROM a JOIN b ON a.id = b.a_id JOIN c ON b.id = c.b_id ORDER BY a.v, b.id, c.id")
            .unwrap();
        assert_eq!(dp_order.rows(), syntactic_order.rows());
        assert!(!dp_order.rows().is_empty(), "テストの前提として一致する行が無ければ意味が無い");
    }

    #[test]
    fn cartesian_product_is_used_only_when_no_connected_extension_exists() {
        // AとCの間に`ON`条件が無い3テーブル。Bを介した経路も無いよう、
        // Bの結合条件も外す(A・B・Cの3つが互いに独立)。DPはCartesian
        // Product(NestedLoopJoin、条件はリテラルtrue)を使わざるを得ない。
        let catalog = abc_catalog();
        let physical = optimize_sql("SELECT a.v FROM a JOIN b ON true JOIN c ON true", &catalog, &NoStats);
        assert!(physical.to_string().contains("NestedLoopJoin"), "plan={physical}");
    }

    #[test]
    fn table_count_beyond_the_limit_falls_back_to_syntactic_order() {
        // MAX_DP_TABLESを超えるテーブル数では、DPを行わず構文順のまま
        // 左深い木を組み立てる。ここでは経路上のテーブル数を検証するのが
        // 目的ではなく、フォールバックが正しく全テーブルを結合できることを
        // 確認する(パニックしない・全テーブルがJoinとして現れる)。
        let mut catalog = Catalog::new();
        for i in 0..=MAX_DP_TABLES {
            catalog
                .create_table(&format!("t{i}"), Schema::new(vec![Column::new("id", DataType::BigInt, false)]))
                .unwrap();
        }
        let mut from_clause = "t0".to_string();
        for i in 1..=MAX_DP_TABLES {
            from_clause.push_str(&format!(" JOIN t{i} ON t{}.id = t{i}.id", i - 1));
        }
        let physical = optimize_sql(&format!("SELECT t0.id FROM {from_clause}"), &catalog, &NoStats);
        // MAX_DP_TABLES+1個のテーブル(MAX_DP_TABLES個のJoin)が全部現れる
        // (構文順のフォールバックが1個も落とさず結合し切れている)。
        let join_count = physical.to_string().matches("Join(").count();
        assert_eq!(join_count, MAX_DP_TABLES, "plan={physical}");
    }

    #[test]
    fn optimize_join_order_leaves_two_table_joins_unaffected() {
        // 葉が2個(JOIN1個)のときはこのモジュールを経由しない
        // (`crate::physical_plan::optimize`が2引数の`choose_join_plan`を直接呼ぶ)。
        // ここでは`DEFAULT_ROW_COUNT_ESTIMATE`を経由する行数見積もり(統計無し)が
        // 従来どおり効くことだけを確認する回帰テスト。
        let catalog = abc_catalog();
        let physical = optimize_sql("SELECT a.v FROM a JOIN b ON a.id = b.a_id", &catalog, &NoStats);
        assert!(!physical.to_string().contains("Projection(id"), "2テーブルのJOINでは並べ替え用Projectionを追加しないはず: {physical}");
        assert_eq!(physical_plan::estimate_rows(&physical, &NoStats), DEFAULT_ROW_COUNT_ESTIMATE);
    }
}

