//! `LogicalPlan`から**Physical Plan**を作り、木を根から葉へ`next()`で1行ずつ
//! 引っ張り出す**Volcanoモデル**のPull型Executorで実行する。
//!
//! # Logical Planの限界
//!
//! 第18章の`Database::eval_query_plan`は、`LogicalPlan`の各ノードを子から
//! 順に評価しながら、結果を`Vec<Tuple>`として丸ごと組み立て直す再帰関数だった。
//! `Filter`は子の`Vec<Tuple>`を受け取ってから絞り込んだ`Vec<Tuple>`を新しく
//! 作り、`Projection`はその結果をさらに受け取って新しい`Vec<Tuple>`を作る。
//! テーブルが10,000行あり、`WHERE`がそのうち1行しか残さない`SELECT`でも、
//! `Filter`が返す前の中間結果は最大10,000行分の`Tuple`を一度にメモリへ載せる。
//! `LogicalPlan`自体は「`Filter`の後に`Projection`が続く」という順序しか
//! 決めておらず、その順序をどう実行するか(1行ずつ流すか、段ごとに丸ごと
//! 作り直すか)は、`eval_query_plan`というRustの1関数の書き方が決めていた。
//!
//! この章では、この「段ごとに丸ごと作り直す」実行方式を、行を1件ずつ引き渡す
//! Pull型の実行方式に置き換える。各演算子は共通の[`Executor`] traitを実装し、
//! 呼び出し元が`next()`を呼ぶたびに、自分の子の`next()`を必要な分だけ呼んで
//! (`Filter`であれば「条件に一致する1行が見つかるまで」)ちょうど1行を返す。
//! `next()`の呼び出しは根から葉へ下り、行は葉から根へ返っていく。この実行方式を
//! **Volcanoモデル**(Pull型実行)と呼ぶ([第2章](../ch02-life-of-a-query.md)で
//! 概観したとおり)。
//!
//! # `PhysicalPlan`: 実行アルゴリズムを確定した木
//!
//! `LogicalPlan`は「何を計算するか」だけを表し、`Scan`が全件走査になるのか
//! 索引を使うのかを決めていなかった(第18章)。この章の`PhysicalPlan`は、その
//! 決定を確定させた木である。索引はまだこのクレートに無いため、`Scan`は
//! 常に[`PhysicalPlan::SeqScan`](Sequential Scan)へ変換される。`optimize`と
//! いう1つの関数がこの変換を担うが、この章の時点では「選択肢が1つしか無いので
//! 選びようがない」変換にすぎない。第25章でIndex Scanが加わったとき、
//! `optimize`は`Scan`を`SeqScan`と`IndexScan`のどちらかへ振り分ける、本当の
//! 意味での選択を行うようになる。この章で先に`LogicalPlan`と型を分けておくのは、
//! その将来の選択が入り込む場所を、実行(`Executor`)とは別の層として今のうちに
//! 用意しておくためである。
//!
//! # `INSERT`・`UPDATE`・`DELETE`は`Executor`にしない
//!
//! `PhysicalPlan::Insert`・`Update`・`Delete`はこの木の一部として存在し、
//! `EXPLAIN`の出力にもノードとして現れる。しかし`SeqScan`・`Filter`・
//! `Projection`と違い、これらを実際に実行するのは[`Executor`]ではない
//! (`Database::execute_insert`等が、第18章までと同じ`executor::insert`・
//! `storage_insert`等をそのまま呼ぶ)。理由は2つある。
//!
//! 1. **全件検査してから全件書き込む不変条件を保ちたい**。`INSERT`の3行目が
//!    `NOT NULL`制約に違反していたら、1・2行目がすでに検査を通っていても
//!    1行も書き込まない、という「全部か無か」の性質は第10章から一貫している
//!    (`executor::plan_insert_rows`のドキュメント参照)。`next()`が1行ごとに
//!    即座に書き込む設計にすると、この性質を保つには「書き込み前に全部
//!    バッファする」という段階を`Executor`の外にもう1つ用意する必要があり、
//!    Pull型実行の利点(段階を1つに減らす)を打ち消してしまう。
//! 2. **書き込みには`&mut`の排他アクセスが要る**。`UPDATE`・`DELETE`が
//!    Volcanoの子として`SeqScan`を持つ設計にすると、`UpdateExec`は
//!    「読み取り用に子`Executor`が持つ`&Storage`」と「書き込み用の
//!    `&mut Storage`」を同時に必要とすることになる。子`Executor`は
//!    `next()`を呼び終えるまで`&Storage`を手放さないため、この2つを
//!    1つの構造体に共存させることはRustの借用規則の範囲では書けない
//!    (`Storage::scan`が返す`Scan<'a>`は`&'a Storage`を借りたままの
//!    イテレータである)。`executor::storage_update`(第16章)がすでに
//!    「`scan`で全件読み切ってから`update`で書き込む」という2段階の
//!    関数として実装済みであり、この章で新たに書き直す理由が無い。
//!
//! `SELECT`(読み取りのみ)は、この制約を持たない。行を書き換えないので、
//! `&Backend`という共有参照だけで木全体を組み立てられ、`Filter`・
//! `Projection`は純粋に「子から1行受け取って加工する」だけの演算子になる。
//! この非対称性(読み取りは真にストリーミング、書き込みは検証してから
//! 一括反映)こそがこの章の設計判断であり、`INSERT`・`UPDATE`・`DELETE`を
//! Volcanoの子として分解しなかった理由である。

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::ops::Bound;
use std::rc::Rc;

use crate::ast::{AggregateFunc, BinaryOperator, Expr, JoinKind, UnaryOperator};
use crate::binder::{AggregateCall, BoundAssignment, BoundExpr, BoundSelectItem};
use crate::btree::RangeScan;
use crate::cost_model;
use crate::error::{DbError, DbResult};
use crate::estimator;
use crate::eval::{FunctionRegistry, eval_bound_expr, eval_expr};
use crate::executor::predicate_matches;
use crate::heap_file::Scan as HeapScan;
use crate::ids::{RecordId, TableId};
use crate::logical_plan::{self, LogicalPlan, SortKey};
use crate::statistics::{ColumnStats, TableStats};
use crate::storage::Storage;
use crate::storage_mem::MemTable;
use crate::tuple_codec::decode_tuple;
use crate::types::{DataType, Row, Schema, Tuple, Value, compare_values};

// ==================================================================
// PhysicalPlan: EXPLAINが表示する、実行アルゴリズムを確定した木
// ==================================================================

/// 実行アルゴリズムを確定した演算子1個。
///
/// `LogicalPlan`と1対1に対応するが、`Scan`は`SeqScan`という具体的な
/// アルゴリズム名に変わる。この章では`SeqScan`以外の選択肢が無いため
/// [`optimize`]は形を変えるだけの変換だが、型としては`LogicalPlan`から
/// 独立させてある(モジュール冒頭の説明を参照)。
#[derive(Debug, Clone, PartialEq)]
pub enum PhysicalPlan {
    SeqScan(SeqScanNode),
    /// `WHERE`の連言から索引の効く述語(Point・Range)を抽出できたときに
    /// `SeqScan`の代わりに選ばれる(第25章、`choose_access_path`)。
    IndexScan(IndexScanNode),
    Values(ValuesNode),
    Filter(FilterNode),
    /// `ON`が等値条件の連言(AND)へ分解できるときに選ばれる(第22章、
    /// `split_equi_join_keys`)。
    HashJoin(HashJoinNode),
    /// `ON`が単一の等値条件で、内側テーブルの結合列に索引があるときに
    /// `HashJoin`より優先して選ばれる(第25章)。
    IndexNestedLoopJoin(IndexNestedLoopJoinNode),
    /// `ON`が任意の条件のときに選ばれる、Joinの基準実装(第22章)。
    NestedLoopJoin(NestedLoopJoinNode),
    Aggregate(AggregateNode),
    Projection(ProjectionNode),
    Distinct(DistinctNode),
    Sort(SortNode),
    Limit(LimitNode),
    Insert(InsertNode),
    Update(UpdateNode),
    Delete(DeleteNode),
}

/// [`PhysicalPlan::SeqScan`]が持つ情報。`LogicalPlan::Scan`(`ScanNode`)と
/// 同じ形だが、「テーブル全体を先頭から読む」という具体的な走査方法を選んだ
/// ことを名前で表す。
#[derive(Debug, Clone, PartialEq)]
pub struct SeqScanNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
}

/// [`PhysicalPlan::IndexScan`]が選んだアクセスパスの種類(第25章)。
///
/// `Point`は`col = 定数`から作る。`Range`は`col > / >= / < / <=`から作った
/// `Bound<Value>`の対で、`std::ops::Bound`の意味は[`crate::btree::BTree::range`]
/// (第24章)と同じ(`Bound::Unbounded`はその側に制限が無いことを表す)。
/// `col = NULL`のような、`NULL`を値として持つ述語はどちらにも現れない
/// (`crate::btree::BTree`が`NULL`をキーに持てないため、`choose_access_path`が
/// 抽出の対象から外し、`Filter`に残す。本文の解説を参照)。
#[derive(Debug, Clone, PartialEq)]
pub enum IndexScanKind {
    Point(Value),
    Range { lower: Bound<Value>, upper: Bound<Value> },
}

/// [`PhysicalPlan::IndexScan`]が持つ情報(第25章)。`table_id`・`table_name`・
/// `schema`は[`SeqScanNode`]と同じ形(このノードが置き換える対象がまさに
/// `SeqScan`であることを表す)。`index_name`は`crate::storage::Storage`が
/// 保持する索引名で、`IndexScanExec`がこの名前で`Storage::index_btree`を
/// 引く。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexScanNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub index_name: String,
    pub column_name: String,
    pub kind: IndexScanKind,
}

/// [`PhysicalPlan::Values`]が持つ情報。`LogicalPlan::Values`と同じ形。
#[derive(Debug, Clone, PartialEq)]
pub struct ValuesNode {
    pub schema: Schema,
    pub rows: Vec<Vec<Expr>>,
}

/// [`PhysicalPlan::Filter`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct FilterNode {
    pub input: Box<PhysicalPlan>,
    pub predicate: BoundExpr,
}

/// [`PhysicalPlan::NestedLoopJoin`]が持つ情報(第22章)。`condition`は
/// `LogicalPlan::Join::condition`をそのまま引き継ぐ(左右を連結した
/// 結合後スキーマ上のフラットな`column_index`を持つ)。
#[derive(Debug, Clone, PartialEq)]
pub struct NestedLoopJoinNode {
    pub left: Box<PhysicalPlan>,
    pub right: Box<PhysicalPlan>,
    pub kind: JoinKind,
    pub condition: BoundExpr,
}

/// [`PhysicalPlan::HashJoin`]が持つ情報(第22章)。
///
/// `keys`は`condition`(等値条件の連言)から取り出した`(left_key, right_key)`の
/// 対の並びで、結合キーが複数列でも(`a.x = b.x AND a.y = b.y`)全て保持する。
/// `left_key`は`left`の出力(結合後スキーマの左半分)に対する添字のまま、
/// `right_key`は`right`単体の出力に対する添字へシフト済み(`optimize`の
/// `shift_column_index`)である。これは、Build段階(`right`をそのまま
/// 1件ずつ読んで鍵を計算する)が`right`自身の`Executor`が返す行(左側を
/// まだ連結していない、`right`単体のスキーマを持つ行)に対して鍵を評価する
/// 必要があるためである。`condition`は`EXPLAIN`での表示にだけ使い、
/// 実行(`HashJoinExec`)は`keys`だけを見る。
#[derive(Debug, Clone, PartialEq)]
pub struct HashJoinNode {
    pub left: Box<PhysicalPlan>,
    pub right: Box<PhysicalPlan>,
    pub kind: JoinKind,
    pub keys: Vec<(BoundExpr, BoundExpr)>,
    pub condition: BoundExpr,
}

/// [`PhysicalPlan::IndexNestedLoopJoin`]が持つ情報(第25章)。
///
/// `HashJoin`と違い、内側テーブル(`right`)を独立した`PhysicalPlan`のまま
/// 保持しない。内側の行は、`left`の行1件ごとに`outer_key`を評価し、その値で
/// `index_name`が指す索引を`lookup`することで初めて決まる(HashJoinの
/// Build段階のように内側の全行を先に読み切ることはしない)。`table_id`・
/// `table_name`・`schema`は内側テーブルの情報で、索引から得た`RecordId`を
/// Heapから`fetch`するために使う。
///
/// `outer_key`は`left`の出力(結合後スキーマの左半分)に対する式で、
/// `HashJoinNode::keys`の`left_key`と同じくシフト不要である(理由は
/// `HashJoinNode`のドキュメントを参照)。`condition`は`EXPLAIN`表示専用で、
/// 実行(`IndexNestedLoopJoinExec`)は`outer_key`と`index_name`だけを見る。
#[derive(Debug, Clone, PartialEq)]
pub struct IndexNestedLoopJoinNode {
    pub left: Box<PhysicalPlan>,
    pub kind: JoinKind,
    pub condition: BoundExpr,
    pub outer_key: BoundExpr,
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub index_name: String,
    pub column_name: String,
}

/// [`PhysicalPlan::Aggregate`]が持つ情報(第21章)。`LogicalPlan::Aggregate`と
/// 同じ形。
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateNode {
    pub input: Box<PhysicalPlan>,
    pub group_by: Vec<BoundExpr>,
    pub calls: Vec<AggregateCall>,
    pub schema: Schema,
}

/// [`PhysicalPlan::Projection`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionNode {
    pub input: Box<PhysicalPlan>,
    pub projection: Vec<BoundSelectItem>,
}

/// [`PhysicalPlan::Distinct`]が持つ情報(第21章)。
#[derive(Debug, Clone, PartialEq)]
pub struct DistinctNode {
    pub input: Box<PhysicalPlan>,
}

/// [`PhysicalPlan::Sort`]が持つ情報(第21章)。
#[derive(Debug, Clone, PartialEq)]
pub struct SortNode {
    pub input: Box<PhysicalPlan>,
    pub keys: Vec<SortKey>,
}

/// [`PhysicalPlan::Limit`]が持つ情報(第21章)。
#[derive(Debug, Clone, PartialEq)]
pub struct LimitNode {
    pub input: Box<PhysicalPlan>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// [`PhysicalPlan::Insert`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct InsertNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub columns: Option<Vec<usize>>,
    pub input: Box<PhysicalPlan>,
}

/// [`PhysicalPlan::Update`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub assignments: Vec<BoundAssignment>,
    pub predicate: Option<BoundExpr>,
    pub input: Box<PhysicalPlan>,
}

/// [`PhysicalPlan::Delete`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub predicate: Option<BoundExpr>,
    pub input: Box<PhysicalPlan>,
}

/// `LogicalPlan`を`PhysicalPlan`へ変換する。
///
/// `storage`は`Backend::Disk`のときだけ`Some`を渡す(`Database::execute_select`・
/// `execute_explain`を参照)。索引は`crate::storage::Storage`(ディスク
/// バックエンド)だけが持てる(第24章)ため、`storage`が`None`(メモリ
/// バックエンド、または索引を使わない再帰呼び出し)のときは`Scan`が常に
/// `SeqScan`になる。この判断は`Database::memory`が`CREATE INDEX`自体を
/// `DbError::NotImplemented`で拒否している(第24章)こととも整合している。
/// `storage`が`Some`でも、`WHERE`・`ON`に索引の効く述語が無ければやはり
/// `SeqScan`・`HashJoin`・`NestedLoopJoin`が選ばれる。
///
/// `stats`は`crate::cost_model::plan_cost`が推定行数を求めるために使う
/// (`crate::estimator`、第27章)。`ANALYZE`を実行していないテーブルは
/// [`estimator::DEFAULT_ROW_COUNT_ESTIMATE`]にフォールバックする(第27章から
/// 変更していない)。
///
/// アクセスパス・Join方式の選択は、第25章の固定優先順位(Point > Range > Seq、
/// Index Nested Loop Join > Hash Join)をやめ、**候補をすべて構築してから
/// [`crate::cost_model::plan_cost`]でコストを見積もり、最小のものを選ぶ**
/// 方式にした(第28章、`choose_scan_plan`・`choose_join_plan`)。`JOIN`を挟む
/// クエリの`WHERE`は、この章でもまだ個々のテーブルへ押し下げない
/// (Predicate Pushdownは第26章)。
pub fn optimize(plan: LogicalPlan, storage: Option<&Storage>, stats: &dyn StatsLookup) -> PhysicalPlan {
    match plan {
        LogicalPlan::Scan(scan) => PhysicalPlan::SeqScan(SeqScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name,
            schema: scan.schema,
        }),
        LogicalPlan::Values(values) => PhysicalPlan::Values(ValuesNode { schema: values.schema, rows: values.rows }),
        LogicalPlan::Filter(filter) => match (storage, *filter.input) {
            (Some(storage), LogicalPlan::Scan(scan)) => choose_scan_plan(storage, scan, filter.predicate, stats),
            (_, input) => PhysicalPlan::Filter(FilterNode {
                input: Box::new(optimize(input, storage, stats)),
                predicate: filter.predicate,
            }),
        },
        LogicalPlan::Join(join) => {
            // 左深いJoinの連鎖をすべて葉(`Scan`または`Filter(Scan)`)と`ON`条件へ
            // 平らにする(`flatten_join_chain`)。葉が3個以上(`JOIN`が2個以上)
            // なら、この段全体を[`join_order::optimize_join_order`](第29章)が
            // 引き取り、構文順とは限らない左深い木を組み立てる。葉が2個
            // (`JOIN`1個)なら、探索する順序の余地が無いため、第22〜28章までと
            // 同じ経路(このまま2引数の[`choose_join_plan`])で済ませる。
            let mut leaves = Vec::new();
            let mut conditions = Vec::new();
            flatten_join_chain(LogicalPlan::Join(join), &mut leaves, &mut conditions);
            if leaves.len() >= 3 {
                crate::join_order::optimize_join_order(leaves, conditions, storage, stats)
            } else {
                let mut leaves = leaves.into_iter();
                let left = optimize(leaves.next().expect("葉は2個以上"), storage, stats);
                let right = optimize(leaves.next().expect("葉は2個以上"), storage, stats);
                let condition = conditions.into_iter().next().expect("JOIN1個の条件は必ず1個");
                let left_len = left.output_schema().len();
                match split_equi_join_keys(&condition, left_len) {
                    Some(keys) => {
                        let keys: Vec<(BoundExpr, BoundExpr)> = keys
                            .into_iter()
                            .map(|(left_key, right_key)| (left_key, shift_column_index(&right_key, left_len)))
                            .collect();
                        choose_join_plan(storage, stats, left, right, JoinKind::Inner, condition, keys)
                    }
                    None => PhysicalPlan::NestedLoopJoin(NestedLoopJoinNode {
                        left: Box::new(left),
                        right: Box::new(right),
                        kind: JoinKind::Inner,
                        condition,
                    }),
                }
            }
        }
        LogicalPlan::Aggregate(aggregate) => PhysicalPlan::Aggregate(AggregateNode {
            input: Box::new(optimize(*aggregate.input, storage, stats)),
            group_by: aggregate.group_by,
            calls: aggregate.calls,
            schema: aggregate.schema,
        }),
        LogicalPlan::Projection(projection) => PhysicalPlan::Projection(ProjectionNode {
            input: Box::new(optimize(*projection.input, storage, stats)),
            projection: projection.projection,
        }),
        LogicalPlan::Distinct(distinct) => {
            PhysicalPlan::Distinct(DistinctNode { input: Box::new(optimize(*distinct.input, storage, stats)) })
        }
        LogicalPlan::Sort(sort) => {
            let input = optimize(*sort.input, storage, stats);
            // Required Ordering(`sort.keys`)が`input`のPhysical Property
            // (`output_ordering`)ですでに満たされていれば、`Sort`そのものを
            // 積まずに`input`をそのまま返す(第29章、`sort_is_already_satisfied`)。
            if sort_is_already_satisfied(&sort.keys, &input) {
                input
            } else {
                PhysicalPlan::Sort(SortNode { input: Box::new(input), keys: sort.keys })
            }
        }
        LogicalPlan::Limit(limit) => PhysicalPlan::Limit(LimitNode {
            input: Box::new(optimize(*limit.input, storage, stats)),
            limit: limit.limit,
            offset: limit.offset,
        }),
        LogicalPlan::Insert(insert) => PhysicalPlan::Insert(InsertNode {
            table_id: insert.table_id,
            table_name: insert.table_name,
            schema: insert.schema,
            columns: insert.columns,
            input: Box::new(optimize(*insert.input, storage, stats)),
        }),
        LogicalPlan::Update(update) => PhysicalPlan::Update(UpdateNode {
            table_id: update.table_id,
            table_name: update.table_name,
            schema: update.schema,
            assignments: update.assignments,
            predicate: update.predicate,
            input: Box::new(optimize(*update.input, storage, stats)),
        }),
        LogicalPlan::Delete(delete) => PhysicalPlan::Delete(DeleteNode {
            table_id: delete.table_id,
            table_name: delete.table_name,
            schema: delete.schema,
            predicate: delete.predicate,
            input: Box::new(optimize(*delete.input, storage, stats)),
        }),
    }
}

/// 左深い`Join`の連鎖を、`n`個の葉(`Scan`または`Filter(Scan)`)と`n-1`個の
/// `ON`条件へ平らにする(第29章)。
///
/// `crate::logical_plan::build_from`が組み立てる木は常に`((t0 JOIN t1) JOIN
/// t2) JOIN ...`という形で、`right`は常にその段で新しく加わった1個の葉、
/// `left`はさらに`Join`か最初の葉である。`rules::optimize`のPredicate
/// Pushdown(第26章)は`left`・`right`の直上に`Filter`を追加することはあっても
/// `Join`の構造そのもの(どの2つが結合されるか)は変えないため、この前提は
/// 物理計画への変換時点でも保たれている。`JoinKind`はこのcrateでは`Inner`
/// しか無い(`crate::ast::JoinKind`)ため、`kind`は引き継がず呼び出し側が
/// `JoinKind::Inner`を使う。
pub(crate) fn flatten_join_chain(plan: LogicalPlan, leaves: &mut Vec<LogicalPlan>, conditions: &mut Vec<BoundExpr>) {
    match plan {
        LogicalPlan::Join(join) => {
            flatten_join_chain(*join.left, leaves, conditions);
            conditions.push(join.condition);
            leaves.push(*join.right);
        }
        other => leaves.push(other),
    }
}

/// 複数の候補`PhysicalPlan`から、[`crate::cost_model::plan_cost`]が最小になる
/// ものを選ぶ。`candidates`は空であってはならない(呼び出し元が必ず1個以上を
/// 積む)。コストが等しい場合は出現順で最初の候補を選ぶ(`Iterator::min_by`と
/// 同じ「最初に見つかった最小値を残す」規則、第25章までの優先順位に代わる
/// 決定的な同点処理)。
pub(crate) fn cheapest(candidates: Vec<PhysicalPlan>, stats: &dyn StatsLookup, storage: Option<&Storage>) -> PhysicalPlan {
    candidates
        .into_iter()
        .min_by(|a, b| {
            cost_model::plan_cost(a, stats, storage).value().partial_cmp(&cost_model::plan_cost(b, stats, storage).value()).expect(
                "コストはNaN・無限大にならない(行数・ページ数はすべて有限のu64からf64へ変換した値であるため)",
            )
        })
        .expect("candidatesは呼び出し元が必ず1個以上積む")
}

// ------------------------------------------------------------------
// Scan演算子の物理選択: WHEREの連言から索引の効く述語を抽出する(第25章)
// ------------------------------------------------------------------

/// `scan`(索引を検討する対象のテーブル)に対する`predicate`(`WHERE`の全体)
/// から作れるアクセスパス候補を、`PhysicalPlan`として組み立てられる形式
/// (索引を吸収した残差条件があれば`Filter`で包んだ木)ですべて列挙する。
///
/// 候補は次の3種類である。
///
/// 1. `predicate`をANDで分解した連言(conjunct)のうち、索引付き列への
///    等値比較1つひとつを取り出したPoint候補(条件が複数あれば、条件の数
///    だけ候補ができる)。
/// 2. 索引付き列ごとに下限・上限の候補をまとめたRange候補(列の数だけ
///    候補ができる。同じ列に複数の下限・上限があっても、出現順で最初の
///    1本だけを境界に使う。使わなかった側は残差条件として`Filter`に
///    残るので正しさには影響しない、第25章の解説を参照)。
/// 3. 索引を一切使わない、`SeqScan`の上に`predicate`全体を載せた候補
///    (常に1つ、索引が無くても必ず選べる安全な既定)。
///
/// `col = NULL`のような`NULL`値を持つ述語はPoint・Rangeどちらの候補からも
/// 除外する(`crate::btree::BTree`が`NULL`をキーに持てないため、第25章)。
/// 呼び出し元(`choose_scan_plan`)がこれらをコストで比較し、最小のものを選ぶ。
fn scan_plan_candidates(storage: &Storage, scan: &logical_plan::ScanNode, predicate: BoundExpr) -> Vec<PhysicalPlan> {
    let mut conjuncts: Vec<&BoundExpr> = Vec::new();
    collect_conjuncts(&predicate, &mut conjuncts);

    let mut candidates = Vec::new();

    // Point候補: 索引付き列への等値比較を、出現するだけすべて候補にする。
    for (i, conjunct) in conjuncts.iter().enumerate() {
        let Some((column_index, op, value)) = as_column_literal_comparison(conjunct) else { continue };
        if op != BinaryOperator::Eq || value.is_null() {
            continue;
        }
        let Some(info) = storage.index_for_column(scan.table_id, column_index) else { continue };
        let remaining = rebuild_conjunction(exclude(&conjuncts, &[i]));
        let node = IndexScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name.clone(),
            schema: scan.schema.clone(),
            index_name: info.name.clone(),
            column_name: info.column_name.clone(),
            kind: IndexScanKind::Point(value),
        };
        candidates.push(wrap_index_scan(node, remaining));
    }

    // Range候補: 列ごとに下限・上限の候補を集め、索引がある列ごとに1候補作る。
    let mut range_order: Vec<usize> = Vec::new();
    let mut ranges: HashMap<usize, RangeAccum> = HashMap::new();
    for (i, conjunct) in conjuncts.iter().enumerate() {
        let Some((column_index, op, value)) = as_column_literal_comparison(conjunct) else { continue };
        if op == BinaryOperator::Eq || value.is_null() {
            continue;
        }
        let is_lower = matches!(op, BinaryOperator::Gt | BinaryOperator::GtEq);
        let bound = match op {
            BinaryOperator::Gt => Bound::Excluded(value),
            BinaryOperator::GtEq => Bound::Included(value),
            BinaryOperator::Lt => Bound::Excluded(value),
            BinaryOperator::LtEq => Bound::Included(value),
            _ => continue,
        };
        let accum = ranges.entry(column_index).or_insert_with(|| {
            range_order.push(column_index);
            RangeAccum::default()
        });
        if is_lower {
            accum.lower.get_or_insert((i, bound));
        } else {
            accum.upper.get_or_insert((i, bound));
        }
    }

    for column_index in range_order {
        let Some(info) = storage.index_for_column(scan.table_id, column_index) else { continue };
        let accum = ranges.remove(&column_index).expect("range_orderに積んだ列は必ずrangesに存在する");
        let mut used = Vec::new();
        let lower = match accum.lower {
            Some((i, bound)) => {
                used.push(i);
                bound
            }
            None => Bound::Unbounded,
        };
        let upper = match accum.upper {
            Some((i, bound)) => {
                used.push(i);
                bound
            }
            None => Bound::Unbounded,
        };
        let remaining = rebuild_conjunction(exclude(&conjuncts, &used));
        let node = IndexScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name.clone(),
            schema: scan.schema.clone(),
            index_name: info.name.clone(),
            column_name: info.column_name.clone(),
            kind: IndexScanKind::Range { lower, upper },
        };
        candidates.push(wrap_index_scan(node, remaining));
    }

    // conjunctsは`predicate`を参照するだけの借用なので、ここまでで使い終える。
    // SeqScan候補はこの章でも常に安全な既定として残す(索引が1つも
    // 無くても、あるいはすべてのPoint/Range候補よりコストが安ければ選ばれる)。
    drop(conjuncts);
    candidates.push(PhysicalPlan::Filter(FilterNode {
        input: Box::new(PhysicalPlan::SeqScan(SeqScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name.clone(),
            schema: scan.schema.clone(),
        })),
        predicate,
    }));

    candidates
}

/// `node`(索引に吸収された述語)と`remaining`(索引に吸収されなかった
/// 残差条件)から、`PhysicalPlan`を組み立てる。`remaining`が`None`なら
/// `IndexScan`だけ、`Some`ならその上に`Filter`を1段重ねる(第25章と同じ
/// 組み立て方)。
fn wrap_index_scan(node: IndexScanNode, remaining: Option<BoundExpr>) -> PhysicalPlan {
    match remaining {
        Some(predicate) => PhysicalPlan::Filter(FilterNode { input: Box::new(PhysicalPlan::IndexScan(node)), predicate }),
        None => PhysicalPlan::IndexScan(node),
    }
}

/// `scan`の直上に`Filter(predicate)`がある形の`LogicalPlan`を、
/// [`scan_plan_candidates`]が列挙した候補の中からコスト最小のものへ変換する
/// (第28章)。第25章までの固定優先順位(Point > Range > Seq)を置き換える。
fn choose_scan_plan(storage: &Storage, scan: logical_plan::ScanNode, predicate: BoundExpr, stats: &dyn StatsLookup) -> PhysicalPlan {
    let candidates = scan_plan_candidates(storage, &scan, predicate);
    cheapest(candidates, stats, Some(storage))
}

/// [`scan_plan_candidates`]がRange述語を列ごとに集めるための一時的な状態。
/// `lower`・`upper`は「連言の中の何番目か・その境界」の対で、`None`は
/// まだその側の候補が見つかっていないことを表す。
#[derive(Default)]
struct RangeAccum {
    lower: Option<(usize, Bound<Value>)>,
    upper: Option<(usize, Bound<Value>)>,
}

/// `conjuncts`から`skip`に含まれる添字の要素を除いた残りを、`BoundExpr`の
/// 所有権を持つ`Vec`として複製する。
fn exclude(conjuncts: &[&BoundExpr], skip: &[usize]) -> Vec<BoundExpr> {
    conjuncts.iter().enumerate().filter(|(i, _)| !skip.contains(i)).map(|(_, expr)| (*expr).clone()).collect()
}

/// `conjuncts`(すでにANDで分解済み、これ以上分解できない項の並び)を、
/// 左結合の`AND`の木へ組み立て直す。0個なら`None`(残差条件が無い)、1個
/// なら`Filter`を挟まずそのまま使えるようその1個だけを返す。
pub(crate) fn rebuild_conjunction(conjuncts: Vec<BoundExpr>) -> Option<BoundExpr> {
    let mut iter = conjuncts.into_iter();
    let first = iter.next()?;
    Some(iter.fold(first, |acc, next| {
        let span = acc.span();
        BoundExpr::BinaryOp {
            op: BinaryOperator::And,
            lhs: Box::new(acc),
            rhs: Box::new(next),
            data_type: DataType::Boolean,
            span,
        }
    }))
}

/// 式が`column OP literal`または`literal OP column`という形なら、
/// `(column_index, 演算子(常にcolumnを左辺と見た向き), literal値)`を返す。
/// `OP`は`=`・`<`・`<=`・`>`・`>=`のいずれかに限る(`<>`・`AND`・`OR`等は
/// `None`)。`column`は単一テーブルのScan直上のFilterが対象なので、
/// `BoundExpr::ColumnRef::column_index`はそのままそのテーブルの`Schema`上の
/// 添字である(結合後スキーマのオフセットは関係しない、`choose_access_path`の
/// ドキュメントを参照)。
fn as_column_literal_comparison(expr: &BoundExpr) -> Option<(usize, BinaryOperator, Value)> {
    let BoundExpr::BinaryOp { op, lhs, rhs, .. } = strip_paren(expr) else { return None };
    let op = *op;
    if !matches!(
        op,
        BinaryOperator::Eq | BinaryOperator::Lt | BinaryOperator::LtEq | BinaryOperator::Gt | BinaryOperator::GtEq
    ) {
        return None;
    }
    match (column_index_of(lhs), column_index_of(rhs)) {
        (Some(column_index), None) => literal_value(rhs).map(|value| (column_index, op, value)),
        (None, Some(column_index)) => literal_value(lhs).map(|value| (column_index, flip_comparison(op), value)),
        _ => None,
    }
}

fn column_index_of(expr: &BoundExpr) -> Option<usize> {
    match strip_paren(expr) {
        BoundExpr::ColumnRef { column_index, .. } => Some(*column_index),
        _ => None,
    }
}

/// リテラルの`BoundExpr`を`Value`へ変換する。列参照や関数呼び出しなど、
/// 実行時にしか値が定まらない式は`None`(索引述語として使えない)。
fn literal_value(expr: &BoundExpr) -> Option<Value> {
    match strip_paren(expr) {
        BoundExpr::IntLiteral { value, .. } => Some(Value::BigInt(*value)),
        BoundExpr::StringLiteral { value, .. } => Some(Value::Text(value.clone())),
        BoundExpr::BoolLiteral { value, .. } => Some(Value::Boolean(*value)),
        BoundExpr::NullLiteral { .. } => Some(Value::Null),
        _ => None,
    }
}

/// `column OP literal`と`literal OP column`とで、`column`を左辺として
/// 見たときの向きへ演算子を裏返す(`100 <= col`は`col >= 100`)。`=`は
/// 裏返しても`=`のままなので変わらない。
fn flip_comparison(op: BinaryOperator) -> BinaryOperator {
    match op {
        BinaryOperator::Lt => BinaryOperator::Gt,
        BinaryOperator::LtEq => BinaryOperator::GtEq,
        BinaryOperator::Gt => BinaryOperator::Lt,
        BinaryOperator::GtEq => BinaryOperator::LtEq,
        other => other,
    }
}

// ------------------------------------------------------------------
// Join演算子の物理選択: Index Nested Loop Join(第25章)
// ------------------------------------------------------------------

/// `index_scan_target`が見つけた、Index Nested Loop Joinの内側テーブルの
/// 情報。[`IndexNestedLoopJoinNode`]のうち`left`・`kind`・`condition`(呼び
/// 出し側がすでに持っている)以外のフィールドをまとめただけの値である。
struct IndexJoinTarget {
    outer_key: BoundExpr,
    table_id: TableId,
    table_name: String,
    schema: Schema,
    index_name: String,
    column_name: String,
}

/// `keys`(すでにHash Joinの鍵として取り出し済みの等値条件)が、Index
/// Nested Loop Joinとしても実行できるなら、その内側テーブルの情報を返す。
///
/// 条件は3つとも満たす必要がある。
///
/// 1. 等値条件がちょうど1本(`keys.len() == 1`)であること。複数の等値条件
///    (`a.x = b.x AND a.y = b.y`)を1本のB+Tree索引だけで引く手段はこの章には
///    無い(索引キーは単一列に限る、第24章)。
/// 2. `right_key`が(`Paren`で包まれていてもよい)単純な列参照であること。
///    `b.x + 1 = a.y`のような式は索引の値として直接引けない。
/// 3. `right`が`PhysicalPlan::SeqScan`のままであること(`right`側に`Filter`が
///    無い)。`FROM`直下の`JOIN`の右辺には`WHERE`が押し下げられない
///    (`choose_scan_plan`と同じ理由、Predicate Pushdownは第26章)ため、
///    `right`が`Filter`を伴うことはこの章では無い。
fn index_scan_target(
    storage: Option<&Storage>,
    right: &PhysicalPlan,
    keys: &[(BoundExpr, BoundExpr)],
) -> Option<IndexJoinTarget> {
    let storage = storage?;
    let [(left_key, right_key)] = keys else { return None };
    let right_column_index = column_index_of(right_key)?;
    let PhysicalPlan::SeqScan(scan) = right else { return None };
    let info = storage.index_for_column(scan.table_id, right_column_index)?;
    Some(IndexJoinTarget {
        outer_key: left_key.clone(),
        table_id: scan.table_id,
        table_name: scan.table_name.clone(),
        schema: scan.schema.clone(),
        index_name: info.name.clone(),
        column_name: info.column_name.clone(),
    })
}

/// 等値結合の鍵`keys`が取り出せた場合の候補を組み立て、
/// [`crate::cost_model::plan_cost`]が最小のものを選ぶ(第28章)。
///
/// `HashJoin`は常に候補になる(`right`を索引に頼らず全件読み切れる、
/// 第22章)。[`index_scan_target`]が内側テーブルの索引を見つけられた場合は
/// `IndexNestedLoopJoin`も候補に加わる。第25章まではIndex Nested Loop Joinを
/// 見つかり次第無条件に選んでいたが、第25章末の実測が示すとおり、密な結合
/// (内側テーブルのほぼ全行が一致する)ではHash Joinの方が15倍以上速い場合が
/// ある。ここでは2つの候補を実際にコストで比較し、統計情報(第27章)から
/// 見積もった一致行数に応じてどちらが有利かを判断する。
pub(crate) fn choose_join_plan(
    storage: Option<&Storage>,
    stats: &dyn StatsLookup,
    left: PhysicalPlan,
    right: PhysicalPlan,
    kind: JoinKind,
    condition: BoundExpr,
    keys: Vec<(BoundExpr, BoundExpr)>,
) -> PhysicalPlan {
    let index_target = index_scan_target(storage, &right, &keys);

    let mut candidates = Vec::new();
    if let Some(target) = &index_target {
        candidates.push(PhysicalPlan::IndexNestedLoopJoin(IndexNestedLoopJoinNode {
            left: Box::new(left.clone()),
            kind,
            condition: condition.clone(),
            outer_key: target.outer_key.clone(),
            table_id: target.table_id,
            table_name: target.table_name.clone(),
            schema: target.schema.clone(),
            index_name: target.index_name.clone(),
            column_name: target.column_name.clone(),
        }));
    }
    candidates.push(PhysicalPlan::HashJoin(HashJoinNode { left: Box::new(left), right: Box::new(right), kind, keys, condition }));

    cheapest(candidates, stats, storage)
}

// ------------------------------------------------------------------
// Join演算子の物理選択: 等値条件ならHash Join、それ以外はNested Loop Join
// ------------------------------------------------------------------

/// `condition`が、左右をちょうど1個ずつ参照する等値比較の連言(AND)へ
/// 分解できるなら、その`(left_key, right_key)`の対の並びを返す。分解できない
/// (`OR`を含む、比較演算子が`=`以外の項がある、左右どちらか一方だけを
/// 参照しない項がある、など)場合は`None`を返し、呼び出し元(`optimize`)は
/// `NestedLoopJoin`を選ぶ。
///
/// `NestedLoopJoin`だけは、この章でもコストで他候補と比較しない。等値条件が
/// 1つも取り出せない結合(`ON true`のような実質的な直積や、`a.x < b.y`の
/// ような不等号条件)は、`HashJoin`・`IndexNestedLoopJoin`のどちらの実行
/// アルゴリズムにも要求する「等値の鍵」を持たないため、比較する候補が
/// そもそも`NestedLoopJoin`しか無い。
pub(crate) fn split_equi_join_keys(condition: &BoundExpr, left_len: usize) -> Option<Vec<(BoundExpr, BoundExpr)>> {
    let mut conjuncts = Vec::new();
    collect_conjuncts(condition, &mut conjuncts);

    let mut keys = Vec::with_capacity(conjuncts.len());
    for conjunct in conjuncts {
        let BoundExpr::BinaryOp { op: BinaryOperator::Eq, lhs, rhs, .. } = strip_paren(conjunct) else {
            return None;
        };
        let (left_key, right_key) = match (columns_side(lhs, left_len), columns_side(rhs, left_len)) {
            (Some(Side::Left), Some(Side::Right)) => (lhs.as_ref().clone(), rhs.as_ref().clone()),
            (Some(Side::Right), Some(Side::Left)) => (rhs.as_ref().clone(), lhs.as_ref().clone()),
            _ => return None,
        };
        keys.push((left_key, right_key));
    }
    if keys.is_empty() { None } else { Some(keys) }
}

/// `AND`で結ばれた式木を、これ以上`AND`で分解できない項(conjunct)の並びへ
/// 展開する。`Paren`は素通しする(`(a = b) AND (c = d)`のような書き方でも
/// 分解できるようにするため)。
pub(crate) fn collect_conjuncts<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match strip_paren(expr) {
        BoundExpr::BinaryOp { op: BinaryOperator::And, lhs, rhs, .. } => {
            collect_conjuncts(lhs, out);
            collect_conjuncts(rhs, out);
        }
        other => out.push(other),
    }
}

pub(crate) fn strip_paren(expr: &BoundExpr) -> &BoundExpr {
    match expr {
        BoundExpr::Paren { expr, .. } => strip_paren(expr),
        other => other,
    }
}

/// 式が参照する列(`ColumnRef`)の`column_index`が、すべて左側
/// (`< left_len`)か、すべて右側(`>= left_len`)かを判定する。列参照を
/// 1つも含まない式(定数式)や、左右が混在する式は`None`を返す。
///
/// `None`を返す式をHash Joinの鍵にできないのは当然として、列参照を持たない
/// 定数式(`a.x = 1`のような、実質的にJOIN条件ではなくFilter条件)も
/// この章では等値鍵として扱わない。等値鍵は必ず左右それぞれの行から
/// 1つの値を取り出して比較する式であるべきで、定数だけの項を残差条件として
/// 切り出す最適化(Hash Joinの鍵とFilterの併用)はこの章の範囲外である
/// (章末の演習課題)。
pub(crate) fn columns_side(expr: &BoundExpr, left_len: usize) -> Option<Side> {
    let mut side = None;
    if !collect_column_side(expr, left_len, &mut side) {
        return None;
    }
    side
}

/// `expr`の中の列参照を再帰的に辿り、`side`が`None`なら最初に見つかった側を
/// 記録し、以後見つかる列参照がすべて同じ側であれば`true`を返す。異なる側の
/// 列参照が混在した時点で`false`を返す(呼び出し元はこれを「判定不能」として
/// `None`を返す合図に使う)。
fn collect_column_side(expr: &BoundExpr, left_len: usize, side: &mut Option<Side>) -> bool {
    match expr {
        BoundExpr::ColumnRef { column_index, .. } => {
            let this_side = if *column_index < left_len { Side::Left } else { Side::Right };
            match side {
                None => {
                    *side = Some(this_side);
                    true
                }
                Some(existing) => *existing == this_side,
            }
        }
        BoundExpr::IntLiteral { .. } | BoundExpr::StringLiteral { .. } | BoundExpr::BoolLiteral { .. } | BoundExpr::NullLiteral { .. } => {
            true
        }
        BoundExpr::UnaryOp { expr, .. } | BoundExpr::Paren { expr, .. } | BoundExpr::Cast { expr, .. } => {
            collect_column_side(expr, left_len, side)
        }
        BoundExpr::BinaryOp { lhs, rhs, .. } => {
            collect_column_side(lhs, left_len, side) && collect_column_side(rhs, left_len, side)
        }
        BoundExpr::IsNull { expr, .. } => collect_column_side(expr, left_len, side),
        BoundExpr::FunctionCall { args, .. } => args.iter().all(|arg| collect_column_side(arg, left_len, side)),
        BoundExpr::Aggregate { .. } => false, // ON句に集約は現れない(Binderが拒否済み)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Left,
    Right,
}

/// `expr`の中の`ColumnRef::column_index`から`delta`を引いた式を組み立てる。
///
/// `HashJoinNode::keys`の`right_key`は、結合後スキーマ(左を含む)上の
/// フラットな添字のまま`optimize`に渡ってくるが、Build段階は`right`単体の
/// `Executor`が返す行(左側を連結する前の、`right`自身のスキーマを持つ行)に
/// 対してこの鍵を評価する必要がある。この関数は、結合後スキーマ上の添字
/// (`>= left_len`のはず)を`right`単体のスキーマ上のローカルな添字へ
/// 変換するために、木を再帰的に組み立て直す。
pub(crate) fn shift_column_index(expr: &BoundExpr, delta: usize) -> BoundExpr {
    match expr {
        BoundExpr::ColumnRef { table_ordinal, column_index, name, data_type, span } => BoundExpr::ColumnRef {
            table_ordinal: *table_ordinal,
            column_index: column_index - delta,
            name: name.clone(),
            data_type: *data_type,
            span: *span,
        },
        BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. } => expr.clone(),
        BoundExpr::UnaryOp { op, expr, data_type, span } => {
            BoundExpr::UnaryOp { op: *op, expr: Box::new(shift_column_index(expr, delta)), data_type: *data_type, span: *span }
        }
        BoundExpr::BinaryOp { op, lhs, rhs, data_type, span } => BoundExpr::BinaryOp {
            op: *op,
            lhs: Box::new(shift_column_index(lhs, delta)),
            rhs: Box::new(shift_column_index(rhs, delta)),
            data_type: *data_type,
            span: *span,
        },
        BoundExpr::IsNull { expr, negated, span } => {
            BoundExpr::IsNull { expr: Box::new(shift_column_index(expr, delta)), negated: *negated, span: *span }
        }
        BoundExpr::FunctionCall { name, args, data_type, span } => BoundExpr::FunctionCall {
            name: name.clone(),
            args: args.iter().map(|arg| shift_column_index(arg, delta)).collect(),
            data_type: *data_type,
            span: *span,
        },
        BoundExpr::Paren { expr, span } => BoundExpr::Paren { expr: Box::new(shift_column_index(expr, delta)), span: *span },
        BoundExpr::Cast { expr, data_type, span } => {
            BoundExpr::Cast { expr: Box::new(shift_column_index(expr, delta)), data_type: *data_type, span: *span }
        }
        BoundExpr::Aggregate { .. } => expr.clone(), // ON句に現れないので到達しない
    }
}

impl PhysicalPlan {
    /// この演算子が返す行の列構成。ルールは`LogicalPlan::output_schema`と
    /// 同じで、`Projection`の列構成は同じ[`logical_plan::projection_schema`]
    /// を呼んで決める(決め方を2箇所に分けないという第18章の方針を踏襲する)。
    pub fn output_schema(&self) -> Schema {
        match self {
            PhysicalPlan::SeqScan(scan) => scan.schema.clone(),
            PhysicalPlan::IndexScan(scan) => scan.schema.clone(),
            PhysicalPlan::Values(values) => values.schema.clone(),
            PhysicalPlan::Filter(filter) => filter.input.output_schema(),
            PhysicalPlan::NestedLoopJoin(join) => {
                logical_plan::join_schema(&join.left.output_schema(), &join.right.output_schema())
            }
            PhysicalPlan::HashJoin(join) => {
                logical_plan::join_schema(&join.left.output_schema(), &join.right.output_schema())
            }
            PhysicalPlan::IndexNestedLoopJoin(join) => {
                logical_plan::join_schema(&join.left.output_schema(), &join.schema)
            }
            PhysicalPlan::Aggregate(aggregate) => aggregate.schema.clone(),
            PhysicalPlan::Projection(projection) => {
                logical_plan::projection_schema(&projection.input.output_schema(), &projection.projection)
            }
            PhysicalPlan::Distinct(distinct) => distinct.input.output_schema(),
            PhysicalPlan::Sort(sort) => sort.input.output_schema(),
            PhysicalPlan::Limit(limit) => limit.input.output_schema(),
            PhysicalPlan::Insert(_) | PhysicalPlan::Update(_) | PhysicalPlan::Delete(_) => Schema::new(Vec::new()),
        }
    }

    /// この演算子の直接の子。第27章の`EXPLAIN`/`EXPLAIN ANALYZE`が、この木と
    /// 同じ形を持つ推定行数・実測行数の木([`estimate_rows`]・`CounterNode`)を
    /// 組み立てるために`pub(crate)`にしてある。
    pub(crate) fn children(&self) -> Vec<&PhysicalPlan> {
        match self {
            PhysicalPlan::SeqScan(_) | PhysicalPlan::IndexScan(_) | PhysicalPlan::Values(_) => Vec::new(),
            PhysicalPlan::Filter(filter) => vec![&filter.input],
            PhysicalPlan::NestedLoopJoin(join) => vec![&join.left, &join.right],
            PhysicalPlan::HashJoin(join) => vec![&join.left, &join.right],
            // `right`側は独立した`PhysicalPlan`を持たない(`IndexNestedLoopJoinNode`の
            // ドキュメントを参照)。木としての子は`left`だけで、内側テーブルへの
            // 索引アクセスは`write_tree`が合成した1行として表示する(下記)。
            PhysicalPlan::IndexNestedLoopJoin(join) => vec![&join.left],
            PhysicalPlan::Aggregate(aggregate) => vec![&aggregate.input],
            PhysicalPlan::Projection(projection) => vec![&projection.input],
            PhysicalPlan::Distinct(distinct) => vec![&distinct.input],
            PhysicalPlan::Sort(sort) => vec![&sort.input],
            PhysicalPlan::Limit(limit) => vec![&limit.input],
            PhysicalPlan::Insert(insert) => vec![&insert.input],
            PhysicalPlan::Update(update) => vec![&update.input],
            PhysicalPlan::Delete(delete) => vec![&delete.input],
        }
    }

    fn label(&self) -> String {
        match self {
            PhysicalPlan::SeqScan(scan) => format!("SeqScan({})", scan.table_name),
            PhysicalPlan::IndexScan(scan) => {
                format!("IndexScan({}, {})", scan.index_name, fmt_index_scan_kind(&scan.column_name, &scan.kind))
            }
            PhysicalPlan::Values(values) => {
                let row_word = if values.rows.len() == 1 { "row" } else { "rows" };
                format!("Values({} {row_word})", values.rows.len())
            }
            PhysicalPlan::Filter(filter) => format!("Filter({})", logical_plan::fmt_bound_expr(&filter.predicate)),
            PhysicalPlan::NestedLoopJoin(join) => {
                format!("NestedLoopJoin({}, {})", join.kind.name(), logical_plan::fmt_bound_expr(&join.condition))
            }
            PhysicalPlan::HashJoin(join) => {
                let keys: Vec<String> = join
                    .keys
                    .iter()
                    .map(|(l, r)| format!("{} = {}", logical_plan::fmt_bound_expr(l), logical_plan::fmt_bound_expr(r)))
                    .collect();
                format!("HashJoin({}, {})", join.kind.name(), keys.join(" AND "))
            }
            PhysicalPlan::IndexNestedLoopJoin(join) => {
                format!("IndexNestedLoopJoin({}, {})", join.kind.name(), logical_plan::fmt_bound_expr(&join.condition))
            }
            PhysicalPlan::Aggregate(aggregate) => {
                let group_by: Vec<String> = aggregate.group_by.iter().map(logical_plan::fmt_bound_expr).collect();
                let calls: Vec<String> = aggregate
                    .calls
                    .iter()
                    .map(|call| {
                        let arg = call.arg.as_deref().map(logical_plan::fmt_bound_expr).unwrap_or_else(|| "*".to_string());
                        format!("{}({arg})", call.func.name())
                    })
                    .collect();
                format!("Aggregate(group_by=[{}], calls=[{}])", group_by.join(", "), calls.join(", "))
            }
            PhysicalPlan::Projection(projection) => {
                let items: Vec<&str> = projection.projection.iter().map(|item| item.output_name.as_str()).collect();
                format!("Projection({})", items.join(", "))
            }
            PhysicalPlan::Distinct(_) => "Distinct".to_string(),
            PhysicalPlan::Sort(sort) => {
                let keys: Vec<String> = sort
                    .keys
                    .iter()
                    .map(|key| {
                        let dir = if key.desc { "DESC" } else { "ASC" };
                        format!("{} {dir}", logical_plan::fmt_bound_expr(&key.expr))
                    })
                    .collect();
                format!("Sort({})", keys.join(", "))
            }
            PhysicalPlan::Limit(limit) => match (limit.limit, limit.offset) {
                (Some(n), Some(o)) => format!("Limit(limit={n}, offset={o})"),
                (Some(n), None) => format!("Limit(limit={n})"),
                (None, Some(o)) => format!("Limit(offset={o})"),
                (None, None) => "Limit".to_string(),
            },
            PhysicalPlan::Insert(insert) => format!("Insert({})", insert.table_name),
            PhysicalPlan::Update(update) => format!("Update({})", update.table_name),
            PhysicalPlan::Delete(delete) => format!("Delete({})", delete.table_name),
        }
    }

    fn write_tree(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        if depth == 0 {
            writeln!(f, "{}", self.label())?;
        } else {
            let indent = "  ".repeat(depth);
            writeln!(f, "{indent}└─ {}", self.label())?;
        }
        for child in self.children() {
            child.write_tree(f, depth + 1)?;
        }
        // `IndexNestedLoopJoin`の内側テーブルは独立した`PhysicalPlan`(木の子)
        // としては存在しない(`IndexNestedLoopJoinNode`のドキュメント、
        // `children`を参照)。木としての深さはここで手動に1段掘り、`left`と
        // 並ぶ形の1行として「どの索引をどの外側キーで引くか」を表示する。
        if let PhysicalPlan::IndexNestedLoopJoin(join) = self {
            let indent = "  ".repeat(depth + 1);
            writeln!(
                f,
                "{indent}└─ IndexScan({}, {} = {})",
                join.index_name,
                join.column_name,
                logical_plan::fmt_bound_expr(&join.outer_key)
            )?;
        }
        Ok(())
    }
}

// ==================================================================
// Physical Properties: Required Orderingを満たすIndex Range Scanの出力順序(第29章)
// ==================================================================

/// `plan`の出力が、`plan`自身の出力スキーマ上のどの列について昇順に並んで
/// いるかを返す。並びを保証できる根拠が無ければ`None`(第29章)。
///
/// 並び順という**Physical Property**を持ちうるのは次の演算子だけである。
///
/// * [`PhysicalPlan::IndexScan`]の`Range`(第25章、`BTree::range`が昇順を
///   返す)。`Point`は一致行が常に同じキー値を持つため、「並んでいる」と
///   言っても次段の`Sort`を省略する役には立たず、この章では対象にしない。
/// * [`PhysicalPlan::Sort`]自身(単一列・昇順のときに限る)。
/// * [`PhysicalPlan::Filter`]は行を間引くだけで列の意味も行の相対順序も
///   変えないため、子の順序をそのまま引き継ぐ。
/// * [`PhysicalPlan::Projection`]は、子が持つ順序列がそのまま`ColumnRef`と
///   して出力項目に残っていれば、その出力位置へ付け替えて引き継ぐ。
/// * `NestedLoopJoin`・`HashJoin`・`IndexNestedLoopJoin`は、いずれも
///   `left`の行を1件ずつ`next()`で引いた順序をそのまま外側ループに使う
///   (`NestedLoopJoinExec`・`HashJoinExec`・`IndexNestedLoopJoinExec`の
///   `next`実装を参照)。`right`側は先に`Vec`やハッシュテーブルへ読み切って
///   から中身を引くため、`right`の順序は失われるが、`left`側の順序は
///   結合後スキーマでも同じ列添字のまま(`left`は結合後スキーマの先頭側を
///   占める)保たれる。
///
/// それ以外(`SeqScan`、`HashJoin`のBuild側由来の順序、`Aggregate`、
/// `Distinct`、`Limit`)は順序を保証しない。`SeqScan`はHeap File上の格納順
/// (第13章)を返すだけで、どの列の値とも対応しない。
pub(crate) fn output_ordering(plan: &PhysicalPlan) -> Option<usize> {
    match plan {
        PhysicalPlan::IndexScan(scan) => match &scan.kind {
            IndexScanKind::Range { .. } => scan.schema.index_of(&scan.column_name),
            IndexScanKind::Point(_) => None,
        },
        PhysicalPlan::Filter(filter) => output_ordering(&filter.input),
        PhysicalPlan::Sort(sort) => match sort.keys.as_slice() {
            [key] if !key.desc => match &key.expr {
                BoundExpr::ColumnRef { column_index, .. } => Some(*column_index),
                _ => None,
            },
            _ => None,
        },
        PhysicalPlan::NestedLoopJoin(join) => output_ordering(&join.left),
        PhysicalPlan::HashJoin(join) => output_ordering(&join.left),
        PhysicalPlan::IndexNestedLoopJoin(join) => output_ordering(&join.left),
        PhysicalPlan::Projection(projection) => {
            let input_order = output_ordering(&projection.input)?;
            projection.projection.iter().position(|item| {
                matches!(&item.expr, BoundExpr::ColumnRef { column_index, .. } if *column_index == input_order)
            })
        }
        PhysicalPlan::SeqScan(_)
        | PhysicalPlan::Values(_)
        | PhysicalPlan::Aggregate(_)
        | PhysicalPlan::Distinct(_)
        | PhysicalPlan::Limit(_)
        | PhysicalPlan::Insert(_)
        | PhysicalPlan::Update(_)
        | PhysicalPlan::Delete(_) => None,
    }
}

/// `keys`(`Sort`が要求する並び順、Required Ordering)が、`input`の
/// [`output_ordering`](Physical Property)ですでに満たされているかを判定する。
/// `keys`が単一列・昇順の場合だけを対象にする(この章が扱う最小限の
/// Interesting Order、モジュール冒頭のドキュメントを参照)。
pub(crate) fn sort_is_already_satisfied(keys: &[logical_plan::SortKey], input: &PhysicalPlan) -> bool {
    match keys {
        [logical_plan::SortKey { expr: BoundExpr::ColumnRef { column_index, .. }, desc: false }] => {
            output_ordering(input) == Some(*column_index)
        }
        _ => false,
    }
}

/// [`Value`]を`EXPLAIN`表示用に整形する。`TEXT`だけ引用符を付け、それ以外は
/// `Value`の`Display`実装(`database`モジュールの表形式出力と同じ)をそのまま
/// 使う。`logical_plan::fmt_bound_expr`が`StringLiteral`をこの形で表示するのと
/// 揃えてある。
fn fmt_index_value(value: &Value) -> String {
    match value {
        Value::Text(s) => format!("'{s}'"),
        other => other.to_string(),
    }
}

/// [`IndexScanNode::kind`]を`EXPLAIN`表示用に整形する。
///
/// ```text
/// id = 42                    (Point)
/// amount >= 100 AND amount <= 200   (Range、両端)
/// amount > 100                      (Range、下限だけ)
/// ```
fn fmt_index_scan_kind(column_name: &str, kind: &IndexScanKind) -> String {
    match kind {
        IndexScanKind::Point(value) => format!("{column_name} = {}", fmt_index_value(value)),
        IndexScanKind::Range { lower, upper } => {
            let lower = match lower {
                Bound::Included(v) => Some(format!("{column_name} >= {}", fmt_index_value(v))),
                Bound::Excluded(v) => Some(format!("{column_name} > {}", fmt_index_value(v))),
                Bound::Unbounded => None,
            };
            let upper = match upper {
                Bound::Included(v) => Some(format!("{column_name} <= {}", fmt_index_value(v))),
                Bound::Excluded(v) => Some(format!("{column_name} < {}", fmt_index_value(v))),
                Bound::Unbounded => None,
            };
            [lower, upper].into_iter().flatten().collect::<Vec<_>>().join(" AND ")
        }
    }
}

impl fmt::Display for PhysicalPlan {
    /// `EXPLAIN`が表示する木そのもの。
    ///
    /// ```text
    /// Projection(name)
    ///   └─ Filter(id = 42)
    ///     └─ SeqScan(users)
    /// ```
    ///
    /// `LogicalPlan`の`Display`(第18章)との違いは`Scan`が`SeqScan`という
    /// 具体的なアルゴリズム名で表示される点だけである。この章では他に選択肢が
    /// 無いためこの1点だけの違いだが、第25章で`IndexScan`が選ばれる場合には
    /// ここに現れるラベルが変わる。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_tree(f, 0)
    }
}

// ==================================================================
// Cardinality Estimation: EXPLAIN/EXPLAIN ANALYZEが表示する推定行数(第27章)
// ==================================================================

/// `table_id`から[`TableStats`](第27章)を引ける、統計情報の抽象。
///
/// `crate::binder::CatalogLookup`と同じ考え方で、`Database`が`Backend::Memory`
/// (プロセスのメモリ上だけの`HashMap`)と`Backend::Disk`(`Storage`が
/// Catalogページへ永続化したもの)のどちらを使っていても、`estimate_rows`
/// 自身はどちらのバックエンドかを意識しない。
pub trait StatsLookup {
    /// `table_id`の統計情報を引く。`ANALYZE`を実行していなければ`None`。
    fn table_stats(&self, table_id: TableId) -> Option<&TableStats>;
}

/// 統計情報を一切持たない`StatsLookup`。テストや、統計を無視したい場面で使う。
pub struct NoStats;

impl StatsLookup for NoStats {
    fn table_stats(&self, _table_id: TableId) -> Option<&TableStats> {
        None
    }
}

/// `plan`の根が生成する行数を見積もる。
///
/// `SeqScan`/`IndexScan`は対象テーブルの[`TableStats::row_count`](統計が
/// 無ければ[`estimator::DEFAULT_ROW_COUNT_ESTIMATE`])を返す。`Filter`は
/// 子の推定行数に[`predicate_selectivity`]を掛ける。`Join`系は
/// [`estimator::estimate_join_row_count`]、`Aggregate`は
/// [`estimator::estimate_aggregate_row_count`]を使う。`Insert`・`Update`・
/// `Delete`は`Executor`を経由しない(モジュール冒頭の説明を参照)ため、この章
/// では対象の`input`の推定行数をそのまま返す(影響を受ける行数の見積もり)。
pub fn estimate_rows(plan: &PhysicalPlan, stats: &dyn StatsLookup) -> u64 {
    match plan {
        PhysicalPlan::SeqScan(scan) => table_row_count(stats, scan.table_id),
        PhysicalPlan::IndexScan(scan) => {
            let total = table_row_count(stats, scan.table_id);
            let column_stats = column_stats_of(stats, scan.table_id, &scan.column_name, &scan.schema);
            let selectivity = match &scan.kind {
                IndexScanKind::Point(value) => estimator::estimate_equality_selectivity(column_stats, total, value),
                IndexScanKind::Range { lower, upper } => range_selectivity_of_bounds(column_stats, total, lower, upper),
            };
            ((total as f64) * selectivity).round().max(0.0) as u64
        }
        PhysicalPlan::Values(values) => values.rows.len() as u64,
        PhysicalPlan::Filter(filter) => {
            let input_rows = estimate_rows(&filter.input, stats);
            let selectivity = predicate_selectivity(&filter.predicate, &filter.input, stats);
            ((input_rows as f64) * selectivity).round().max(0.0) as u64
        }
        PhysicalPlan::NestedLoopJoin(join) => {
            // `NestedLoopJoin`は任意の条件(等値とは限らない)を持つため、
            // 結合キー列を特定できない。フォールバック(下記)にすべて委ねる。
            let left_rows = estimate_rows(&join.left, stats);
            let right_rows = estimate_rows(&join.right, stats);
            fallback_join_row_count(left_rows, right_rows)
        }
        PhysicalPlan::HashJoin(join) => {
            let left_rows = estimate_rows(&join.left, stats);
            let right_rows = estimate_rows(&join.right, stats);
            // `HashJoin`は等値条件の対の並び(`keys`)を持つため、先頭のキーの
            // 実際のNDVを引ける(第22章、`split_equi_join_keys`)。複数キーの
            // 場合でも先頭の1本だけを見るのは単純化だが、`AND`で連結された
            // 複数の等値条件は同じかそれ以上に選択的になるはずなので、
            // 先頭キーだけを見た見積もりは「選択されすぎない」側の安全な近似になる。
            let key_stats = join.keys.first().map(|(left_key, right_key)| {
                let left = column_owner_stats(left_key, &join.left, stats).filter(|(c, _)| c.distinct_count > 0);
                let right = column_owner_stats(right_key, &join.right, stats).filter(|(c, _)| c.distinct_count > 0);
                (left, right)
            });
            match key_stats {
                Some((Some((left_col, left_table_rows)), Some((right_col, right_table_rows)))) => {
                    // 結合キーが`NULL`の行は等号で一致しないため、
                    // `estimate_join_row_count`には非NULL行数だけを渡す
                    // (`null_count`/`left_table_rows`から求めた列全体の
                    // NULL率を、Join直前の推定行数へ独立性の仮定で適用する)。
                    let left_non_null = apply_non_null_fraction(left_rows, left_col.null_count, left_table_rows);
                    let right_non_null = apply_non_null_fraction(right_rows, right_col.null_count, right_table_rows);
                    estimator::estimate_join_row_count(left_non_null, right_non_null, left_col.distinct_count, right_col.distinct_count)
                }
                _ => fallback_join_row_count(left_rows, right_rows),
            }
        }
        PhysicalPlan::IndexNestedLoopJoin(join) => {
            let left_rows = estimate_rows(&join.left, stats);
            let inner_rows = table_row_count(stats, join.table_id);
            let column_stats = column_stats_of(stats, join.table_id, &join.column_name, &join.schema);
            let inner_ndv = column_stats.map(|c| c.distinct_count).unwrap_or(inner_rows).max(1);
            let inner_non_null = match column_stats {
                Some(c) => apply_non_null_fraction(inner_rows, c.null_count, inner_rows),
                None => inner_rows,
            };
            estimator::estimate_join_row_count(left_rows, inner_non_null, left_rows.max(1), inner_ndv)
        }
        PhysicalPlan::Aggregate(aggregate) => {
            let input_rows = estimate_rows(&aggregate.input, stats);
            let group_ndvs: Vec<u64> = aggregate
                .group_by
                .iter()
                .map(|expr| column_owner_stats(expr, &aggregate.input, stats).map(|(c, _)| c.distinct_count).unwrap_or(input_rows).max(1))
                .collect();
            estimator::estimate_aggregate_row_count(&group_ndvs, input_rows)
        }
        PhysicalPlan::Projection(projection) => estimate_rows(&projection.input, stats),
        PhysicalPlan::Distinct(distinct) => estimate_rows(&distinct.input, stats),
        PhysicalPlan::Sort(sort) => estimate_rows(&sort.input, stats),
        PhysicalPlan::Limit(limit) => {
            let input_rows = estimate_rows(&limit.input, stats);
            match limit.limit {
                Some(n) => input_rows.min(n as u64),
                None => input_rows,
            }
        }
        PhysicalPlan::Insert(insert) => estimate_rows(&insert.input, stats),
        PhysicalPlan::Update(update) => estimate_rows(&update.input, stats),
        PhysicalPlan::Delete(delete) => estimate_rows(&delete.input, stats),
    }
}

fn table_row_count(stats: &dyn StatsLookup, table_id: TableId) -> u64 {
    stats.table_stats(table_id).map(|s| s.row_count).unwrap_or(estimator::DEFAULT_ROW_COUNT_ESTIMATE)
}

/// `schema`上で`column_name`という名前を持つ列の[`ColumnStats`]を、
/// `table_id`の統計情報から引く。
fn column_stats_of<'a>(
    stats: &'a dyn StatsLookup,
    table_id: TableId,
    column_name: &str,
    schema: &Schema,
) -> Option<&'a crate::statistics::ColumnStats> {
    let index = schema.index_of(column_name)?;
    stats.table_stats(table_id)?.columns.get(index)
}

/// [`IndexScanKind::Range`]の下限・上限の両方から選択率を見積もる。上限・
/// 下限のうち指定されている側だけ[`estimator::estimate_range_selectivity`]を
/// 呼び、両方指定されていれば独立性を仮定した積(`estimate_and_selectivity`)を
/// 取る。`row_count`は対象テーブルの行数(全行基準の選択率を求めるために
/// `estimate_range_selectivity`が必要とする、モジュール冒頭を参照)。
fn range_selectivity_of_bounds(stats: Option<&ColumnStats>, row_count: u64, lower: &Bound<Value>, upper: &Bound<Value>) -> f64 {
    let lower_sel = match lower {
        Bound::Included(v) => Some(estimator::estimate_range_selectivity(stats, row_count, estimator::RangeOp::Ge, v)),
        Bound::Excluded(v) => Some(estimator::estimate_range_selectivity(stats, row_count, estimator::RangeOp::Gt, v)),
        Bound::Unbounded => None,
    };
    let upper_sel = match upper {
        Bound::Included(v) => Some(estimator::estimate_range_selectivity(stats, row_count, estimator::RangeOp::Le, v)),
        Bound::Excluded(v) => Some(estimator::estimate_range_selectivity(stats, row_count, estimator::RangeOp::Lt, v)),
        Bound::Unbounded => None,
    };
    match (lower_sel, upper_sel) {
        (Some(a), Some(b)) => estimator::estimate_and_selectivity(a, b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 1.0,
    }
}

/// 結合キー列のNDVを特定できない場合(`NestedLoopJoin`の任意条件、または
/// `HashJoin`でも列に統計情報が無い場合)の既定の見積もり。「値はすべて
/// 一意」という最も楽観的な既定値(それぞれの出力行数そのもの)を
/// [`estimator::estimate_join_row_count`]へ渡す。主キー・外部キー結合
/// (外部キー側の値がすべて異なる典型的な結合)を想定した単純化である。
fn fallback_join_row_count(left_rows: u64, right_rows: u64) -> u64 {
    estimator::estimate_join_row_count(left_rows, right_rows, left_rows.max(1), right_rows.max(1))
}

/// `rows`件のうち、`null_count`/`table_row_count`から求めた列のNULL率を
/// 差し引いた非NULL行数を見積もる。
///
/// `rows`は結合直前の(`Filter`等を経た後の)推定行数、`null_count`・
/// `table_row_count`は`ANALYZE`が観測したテーブル全体でのNULL率であり、
/// 両者が指す母集団は厳密には異なりうる。`rows`のうち述語で絞り込まれた
/// 部分と、列がNULLである部分が独立に決まるという仮定(この章の他の推定式と
/// 同じ独立性の仮定)のもとで、そのNULL率を`rows`へそのまま適用する。
fn apply_non_null_fraction(rows: u64, null_count: u64, table_row_count: u64) -> u64 {
    let non_null = 1.0 - estimator::null_fraction(null_count, table_row_count);
    ((rows as f64) * non_null).round().max(0.0) as u64
}

/// `plan`の列`column_index`が、どのテーブルのどの列に由来するかを解決する。
///
/// `SeqScan`/`IndexScan`はその場で確定する。`Filter`・`Distinct`・`Sort`・
/// `Limit`は列の意味を変えずに素通しするので、子へそのまま委ねる。`Join`は
/// 左右どちらの出力かを列番号のオフセットで判定し、対応する側へ委ねる
/// (`IndexNestedLoopJoin`の内側テーブルは独立した`PhysicalPlan`を持たない
/// ため、その場で確定する)。`Projection`・`Aggregate`は列の意味が
/// 再構成されるため`None`を返す(この章の推定はここでデフォルトの
/// 選択率にフォールバックする)。
fn resolve_column_owner(plan: &PhysicalPlan, column_index: usize) -> Option<(TableId, usize)> {
    match plan {
        PhysicalPlan::SeqScan(scan) => Some((scan.table_id, column_index)),
        PhysicalPlan::IndexScan(scan) => Some((scan.table_id, column_index)),
        PhysicalPlan::Filter(filter) => resolve_column_owner(&filter.input, column_index),
        PhysicalPlan::Distinct(distinct) => resolve_column_owner(&distinct.input, column_index),
        PhysicalPlan::Sort(sort) => resolve_column_owner(&sort.input, column_index),
        PhysicalPlan::Limit(limit) => resolve_column_owner(&limit.input, column_index),
        PhysicalPlan::NestedLoopJoin(join) => resolve_join_column(&join.left, &join.right, column_index),
        PhysicalPlan::HashJoin(join) => resolve_join_column(&join.left, &join.right, column_index),
        PhysicalPlan::IndexNestedLoopJoin(join) => {
            let left_len = join.left.output_schema().len();
            if column_index < left_len {
                resolve_column_owner(&join.left, column_index)
            } else {
                Some((join.table_id, column_index - left_len))
            }
        }
        PhysicalPlan::Values(_)
        | PhysicalPlan::Aggregate(_)
        | PhysicalPlan::Projection(_)
        | PhysicalPlan::Insert(_)
        | PhysicalPlan::Update(_)
        | PhysicalPlan::Delete(_) => None,
    }
}

fn resolve_join_column(left: &PhysicalPlan, right: &PhysicalPlan, column_index: usize) -> Option<(TableId, usize)> {
    let left_len = left.output_schema().len();
    if column_index < left_len {
        resolve_column_owner(left, column_index)
    } else {
        resolve_column_owner(right, column_index - left_len)
    }
}

/// [`BoundExpr::ColumnRef`]から、その列が由来するテーブルの[`ColumnStats`]と、
/// そのテーブルの行数(`TableStats::row_count`)を引く。列参照でない式、
/// または由来を解決できない式には`None`を返す。行数を併せて返すのは、
/// `estimate_equality_selectivity`・`estimate_range_selectivity`が全行基準の
/// 選択率(モジュール冒頭、`estimator`の説明を参照)を計算するために必要な
/// ためである。
fn column_owner_stats<'a>(expr: &BoundExpr, plan: &PhysicalPlan, stats: &'a dyn StatsLookup) -> Option<(&'a ColumnStats, u64)> {
    let BoundExpr::ColumnRef { column_index, .. } = expr else { return None };
    let (table_id, local_index) = resolve_column_owner(plan, *column_index)?;
    let table_stats = stats.table_stats(table_id)?;
    table_stats.columns.get(local_index).map(|c| (c, table_stats.row_count))
}

/// `predicate`(`plan`を子に持つ`Filter`の述語)の選択率を見積もる。`TRUE`に
/// なる行の割合(`WHERE`句が実際に拾う行の割合)だけを返す、[`predicate_selectivity3`]
/// の薄いラッパー。
///
/// 対応するのは、`col <op> 定数`(`=`・`<>`・`<`・`<=`・`>`・`>=`)の形の比較、
/// `col IS [NOT] NULL`、`AND`・`OR`・`NOT`による組み合わせだけである。列参照が
/// 定数と比較されていない述語(`col1 = col2`、関数呼び出しを含む式など)は、
/// この章の推定式が対応する範囲の外にあるため、
/// [`estimator::DEFAULT_INEQ_SEL`]にフォールバックする。
pub fn predicate_selectivity(predicate: &BoundExpr, plan: &PhysicalPlan, stats: &dyn StatsLookup) -> f64 {
    predicate_selectivity3(predicate, plan, stats).is_true
}

/// [`predicate_selectivity`]の3値論理版。`predicate`の[`estimator::Selectivity3`]
/// (`TRUE`/`FALSE`/`UNKNOWN`の3確率)を式木に沿って再帰的に組み立てる。
///
/// 比較・`IS [NOT] NULL`という葉で`Selectivity3`を作り、`AND`・`OR`・`NOT`は
/// [`estimator::and3`]・[`estimator::or3`]・[`estimator::not3`]でSQLの
/// 真理値表どおりに合成する(`estimator`モジュールの説明を参照)。対応する
/// 範囲の外にある式(列参照が定数と比較されていない述語など)は、
/// `UNKNOWN`にならない([`estimator::Selectivity3::certain`])既定の不等号
/// 選択率として扱う。
fn predicate_selectivity3(predicate: &BoundExpr, plan: &PhysicalPlan, stats: &dyn StatsLookup) -> estimator::Selectivity3 {
    match predicate {
        BoundExpr::Paren { expr, .. } => predicate_selectivity3(expr, plan, stats),
        BoundExpr::UnaryOp { op: UnaryOperator::Not, expr, .. } => estimator::not3(predicate_selectivity3(expr, plan, stats)),
        BoundExpr::BinaryOp { op: BinaryOperator::And, lhs, rhs, .. } => {
            estimator::and3(predicate_selectivity3(lhs, plan, stats), predicate_selectivity3(rhs, plan, stats))
        }
        BoundExpr::BinaryOp { op: BinaryOperator::Or, lhs, rhs, .. } => {
            estimator::or3(predicate_selectivity3(lhs, plan, stats), predicate_selectivity3(rhs, plan, stats))
        }
        BoundExpr::BinaryOp { op, lhs, rhs, .. } => comparison_selectivity3(*op, lhs, rhs, plan, stats),
        BoundExpr::IsNull { expr, negated: false, .. } => {
            let (column_stats, row_count) = column_owner_stats(expr, plan, stats).unzip();
            estimator::is_null_selectivity3(column_stats, row_count.unwrap_or(estimator::DEFAULT_ROW_COUNT_ESTIMATE))
        }
        BoundExpr::IsNull { expr, negated: true, .. } => {
            let (column_stats, row_count) = column_owner_stats(expr, plan, stats).unzip();
            estimator::is_not_null_selectivity3(column_stats, row_count.unwrap_or(estimator::DEFAULT_ROW_COUNT_ESTIMATE))
        }
        _ => estimator::Selectivity3::certain(estimator::DEFAULT_INEQ_SEL),
    }
}

/// `lhs <op> rhs`という1個の比較式の[`estimator::Selectivity3`]を見積もる。
/// `col = 定数`・`定数 = col`のどちらの並びでも同じ選択率になるよう、
/// 列参照がどちらの側にあるかを見て演算子の向きを揃える。
fn comparison_selectivity3(
    op: BinaryOperator,
    lhs: &BoundExpr,
    rhs: &BoundExpr,
    plan: &PhysicalPlan,
    stats: &dyn StatsLookup,
) -> estimator::Selectivity3 {
    let (column_expr, op, value) = match (literal_value(rhs), literal_value(lhs)) {
        (Some(value), _) => (lhs, op, value),
        (None, Some(value)) => (rhs, flip_comparison(op), value),
        (None, None) => return estimator::Selectivity3::certain(estimator::DEFAULT_INEQ_SEL),
    };
    let (column_stats, row_count) = match column_owner_stats(column_expr, plan, stats) {
        Some((c, rc)) => (Some(c), rc),
        None => (None, estimator::DEFAULT_ROW_COUNT_ESTIMATE),
    };
    match op {
        BinaryOperator::Eq => estimator::equality_selectivity3(column_stats, row_count, &value),
        // `<>`は`NOT(=)`そのものなので、`equality_selectivity3`をnot3で
        // 包むだけでよい(独立した近似式を持たない)。
        BinaryOperator::NotEq => estimator::not3(estimator::equality_selectivity3(column_stats, row_count, &value)),
        BinaryOperator::Lt => estimator::range_selectivity3(column_stats, row_count, estimator::RangeOp::Lt, &value),
        BinaryOperator::LtEq => estimator::range_selectivity3(column_stats, row_count, estimator::RangeOp::Le, &value),
        BinaryOperator::Gt => estimator::range_selectivity3(column_stats, row_count, estimator::RangeOp::Gt, &value),
        BinaryOperator::GtEq => estimator::range_selectivity3(column_stats, row_count, estimator::RangeOp::Ge, &value),
        _ => estimator::Selectivity3::certain(estimator::DEFAULT_INEQ_SEL),
    }
}

// ==================================================================
// EXPLAIN / EXPLAIN ANALYZE: 推定行数・実測行数を添えた木の表示(第27章)
// ==================================================================

/// [`CountingExec`]が実測した、`PhysicalPlan`の1ノードぶんの実行時カウンタ。
/// `children`は対応する`PhysicalPlan::children()`と同じ並び順・同じ要素数を
/// 持つ(`build_counter_tree`が`PhysicalPlan`の形をそのまま複製して作る)。
pub struct CounterNode {
    pub count: Rc<Cell<u64>>,
    pub children: Vec<CounterNode>,
}

impl CounterNode {
    /// `plan`と同じ形(子の数・並び順)を持つ、カウンタがすべて0の木を作る。
    pub fn build(plan: &PhysicalPlan) -> CounterNode {
        CounterNode {
            count: Rc::new(Cell::new(0)),
            children: plan.children().iter().map(|child| CounterNode::build(child)).collect(),
        }
    }
}

/// `plan`の`EXPLAIN`表示を組み立てる。`actual`が`Some`なら各行へ
/// ` actual=<実測行数>`も添える(`EXPLAIN ANALYZE`)。`actual`が`None`なら
/// 推定行数(` rows=<推定値>`)だけを添える(従来の`EXPLAIN`)。第28章から、
/// 推定行数の後ろに常に` cost=<推定コスト>`([`crate::cost_model::plan_cost`]、
/// 小数点以下2桁)も添える。`storage`は`plan_cost`がページ数・索引の高さの
/// 実測値を引くために使う(`optimize`の`storage`と同じ、第25章)。
///
/// 木の形・インデント・矢印記法は[`PhysicalPlan`]の`Display`実装
/// (`write_tree`)と同じ規則に従う。`IndexNestedLoopJoin`の内側テーブルの
/// 表示(木の子ではなく、深さを1段手動で掘った合成行)も同様に揃える。
pub fn explain_text(plan: &PhysicalPlan, stats: &dyn StatsLookup, storage: Option<&Storage>, actual: Option<&CounterNode>) -> String {
    let mut out = String::new();
    write_explain_tree(plan, stats, storage, actual, 0, &mut out);
    out
}

fn write_explain_tree(
    plan: &PhysicalPlan,
    stats: &dyn StatsLookup,
    storage: Option<&Storage>,
    actual: Option<&CounterNode>,
    depth: usize,
    out: &mut String,
) {
    let rows = estimate_rows(plan, stats);
    let cost = cost_model::plan_cost(plan, stats, storage);
    if depth == 0 {
        out.push_str(&plan.label());
    } else {
        out.push_str(&"  ".repeat(depth));
        out.push_str("└─ ");
        out.push_str(&plan.label());
    }
    out.push_str(&format!(" rows={rows} cost={:.2}", cost.value()));
    if let Some(node) = actual {
        out.push_str(&format!(" actual={}", node.count.get()));
    }
    out.push('\n');

    for (i, child) in plan.children().iter().enumerate() {
        let child_actual = actual.map(|node| &node.children[i]);
        write_explain_tree(child, stats, storage, child_actual, depth + 1, out);
    }

    if let PhysicalPlan::IndexNestedLoopJoin(join) = plan {
        // `children()`には現れない、内側テーブルへの索引アクセス
        // (`write_tree`と同じ合成行)。実測値を集める`CountingExec`の対象には
        // していない(`IndexNestedLoopJoinExec`は外側の行ごとに内側を
        // `lookup`するため、この1行だけの実測件数を他の演算子と同じ形では
        // 数えられない)ので、推定行数・推定コストだけを添える。
        // 外側の行ごとに異なる値で`lookup`するため、特定の定数に対する選択率
        // ではなく「平均的な等値検索は何行返すか」(`1 / NDV`)を見積もる。
        let inner_rows = table_row_count(stats, join.table_id);
        let column_stats = column_stats_of(stats, join.table_id, &join.column_name, &join.schema);
        let ndv = column_stats.map(|c| c.distinct_count).filter(|&n| n > 0).unwrap_or(inner_rows).max(1);
        let estimated = (inner_rows as f64 / ndv as f64).round().max(0.0) as u64;
        let height = storage.and_then(|storage| storage.index_btree(&join.index_name)).and_then(|btree| btree.height().ok());
        let lookup_cost = cost_model::index_scan_cost(height.map(|h| h as u64).unwrap_or(cost_model::DEFAULT_INDEX_HEIGHT), estimated);
        let indent = "  ".repeat(depth + 1);
        out.push_str(&format!(
            "{indent}└─ IndexScan({}, {} = {}) rows={estimated} cost={:.2}\n",
            join.index_name,
            join.column_name,
            logical_plan::fmt_bound_expr(&join.outer_key),
            lookup_cost.value()
        ));
    }
}

// ==================================================================
// Executor: Volcano型Pull実行
// ==================================================================

/// Physical Planの演算子1個を、Pull型で実行するインターフェース。
///
/// `next()`は呼ばれるたびに、この演算子が生成する行をちょうど1件返す。
/// 生成できる行が尽きたら`Ok(None)`を返し、以後の`next()`呼び出しも
/// `Ok(None)`を返し続ける(呼び出し側は`None`を見た時点でループを止め、
/// 空になった`Executor`へ再度`next()`を呼び直すことはしない)。
///
/// `Box<dyn Executor>`という Trait Object 方式を採用している。演算子ごとに
/// 異なる具象型(`FilterExec`・`ProjectionExec`等)を、`LogicalPlan`の木構造に
/// 対応する形でそのまま入れ子にできるのは、`Box<dyn Executor>`が「同じ
/// インターフェースを持つ異なる型」を1つの`Vec`や1個のフィールドに収められる
/// からである。もう1つの実装候補である Enum Dispatch(`PhysicalPlan`のように
/// 演算子の種類を`enum`で持ち、`next()`の中で`match`する)は、実行時の型情報を
/// 持たない分だけ動的ディスパッチ(vtable経由の関数呼び出し)のコストを避けられる。
/// この章ではTrait Object方式を選んだ。演算子の種類が増えるたびに1個の巨大な
/// `match`式へ`next()`の実装を書き足していくEnum Dispatchより、演算子ごとに
/// 独立した`struct`とその`impl Executor`だけを追加すればよいTrait Object方式の
/// ほうが、今後の章(`Sort`・`Limit`・`Join`等)で演算子の種類を増やしていく
/// このクレートの成長のさせ方に合っている。動的ディスパッチのコストが実際に
/// どれだけ効くかは、この章では測定しない(章末の演習課題に譲る)。
pub trait Executor {
    /// この演算子が返す行の列構成。
    fn output_schema(&self) -> &Schema;
    /// 行をちょうど1件引っ張り出す。尽きたら`Ok(None)`。
    fn next(&mut self) -> DbResult<Option<Tuple>>;
}

/// Values演算子。`VALUES`の各行を構築時にまとめて評価する。
///
/// `Filter`・`Projection`と違い、`ValuesExec`はそもそも「子から1行ずつ引く」
/// 対象を持たない葉であり、`VALUES (1, 'a'), (2, 'b')`のような行数はSQL文の
/// 長さそのものに比例する(せいぜい数十〜数百行)。テーブルの行数のように
/// 無制限に増える値ではないため、構築時に全行を評価してもストリーミング
/// 実行の趣旨(中間結果がテーブル全体に比例しない)を損なわない。
pub struct ValuesExec {
    schema: Schema,
    rows: std::vec::IntoIter<Tuple>,
}

impl ValuesExec {
    pub fn new(schema: Schema, functions: &FunctionRegistry, row_exprs: &[Vec<Expr>]) -> DbResult<Self> {
        let mut rows = Vec::with_capacity(row_exprs.len());
        for exprs in row_exprs {
            let values = exprs.iter().map(|expr| eval_expr(expr, functions, None)).collect::<DbResult<Vec<_>>>()?;
            rows.push(Tuple::new(&schema, values)?);
        }
        Ok(ValuesExec { schema, rows: rows.into_iter() })
    }
}

impl Executor for ValuesExec {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        Ok(self.rows.next())
    }
}

/// Sequential Scan演算子の`MemTable`版。テーブルの行を先頭から順に、
/// 1件ずつ複製して返す。第18章までの`executor::seq_scan`は`table.rows()`を
/// `to_vec()`でまるごと複製していたが、こちらは`std::slice::Iter`を1歩ずつ
/// 進めるだけなので、`next()`が呼ばれた分だけしか複製が起きない。
pub struct MemSeqScanExec<'a> {
    schema: &'a Schema,
    rows: std::slice::Iter<'a, Tuple>,
}

impl<'a> MemSeqScanExec<'a> {
    pub fn new(schema: &'a Schema, table: &'a MemTable) -> Self {
        MemSeqScanExec { schema, rows: table.rows().iter() }
    }
}

impl<'a> Executor for MemSeqScanExec<'a> {
    fn output_schema(&self) -> &Schema {
        self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        Ok(self.rows.next().cloned())
    }
}

/// Sequential Scan演算子の`Storage`版。`Storage::scan`(第15章)が返す
/// `Scan`イテレータは、内部でページを1枚ずつ`BufferPool`から取り出す
/// 遅延評価のイテレータであり、`next()`が呼ばれるまで次のページを読まない。
/// 第18章までの`executor::storage_seq_scan`はこのイテレータを`collect()`して
/// `Vec<Tuple>`へ変換していたが、こちらはそのイテレータをそのまま1件ずつ
/// `decode_tuple`(第13章)へ通すだけで、`Vec`へまとめる段階そのものが無い。
pub struct DiskSeqScanExec<'a> {
    schema: &'a Schema,
    scan: HeapScan<'a>,
}

impl<'a> DiskSeqScanExec<'a> {
    pub fn new(storage: &'a Storage, table_id: TableId, schema: &'a Schema) -> DbResult<Self> {
        Ok(DiskSeqScanExec { schema, scan: storage.scan(table_id)? })
    }
}

impl<'a> Executor for DiskSeqScanExec<'a> {
    fn output_schema(&self) -> &Schema {
        self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        match self.scan.next() {
            None => Ok(None),
            Some(entry) => {
                let (_, bytes) = entry?;
                decode_tuple(self.schema, &bytes).map(Some)
            }
        }
    }
}

/// [`IndexScanExec`]が読み進める順序。Pointは`BTree::lookup`が返した
/// `RecordId`の一覧を先頭から順に、Rangeは[`RangeScan`](第24章)を
/// `next_leaf`のリンクに沿って順にたどる。
enum IndexScanSource<'a> {
    Point(std::vec::IntoIter<RecordId>),
    Range(RangeScan<'a>),
}

/// Point・Range Index Scan演算子(第25章)。`crate::physical_plan::optimize`が
/// `WHERE`から抽出した索引述語(`IndexScanKind`)にもとづき、`crate::btree::BTree`
/// (第23・24章)から`RecordId`を引き、`Storage::get`でHeapから実データを
/// `fetch`する。
///
/// `DiskSeqScanExec`と違い、`Storage::scan`(ページを先頭から順に読む)は
/// 経由しない。索引が返す`RecordId`の並びだけを頼りに、対応するHeapページを
/// 直接読みに行く。索引はディスクバックエンドにしか存在しない(第24章)ため、
/// `MemSeqScanExec`に対応する索引版はこのクレートには無い(モジュール冒頭の
/// `optimize`のドキュメントを参照)。
///
/// **Lazy Delete済みエントリの扱い**: `crate::btree::BTree::delete`と
/// `Storage::index_delete_row`(第24章)により、行を削除すれば索引エントリも
/// 同時に取り除かれるため、通常の運用では索引が指す`RecordId`のHeap行が
/// 存在しないという状況は起こらない。それでも`next()`は`Storage::get`が
/// `None`(該当スロットが空)を返した`RecordId`を無条件にエラーにはせず、
/// 読み飛ばして次の`RecordId`へ進む。索引とHeapの整合性は`Storage`が保つ
/// べき不変条件であり、`IndexScanExec`はそれを信じたうえで、万一の食い違いを
/// panicではなく黙って読み飛ばす形で吸収する(`crate::btree::BTree`の
/// Lazy Deleteという名前が示す「取りこぼしより多少の無駄読みを許す」設計と
/// 対称的な選択である)。
pub struct IndexScanExec<'a> {
    storage: &'a Storage,
    table_id: TableId,
    schema: &'a Schema,
    source: IndexScanSource<'a>,
}

impl<'a> IndexScanExec<'a> {
    pub fn new(storage: &'a Storage, table_id: TableId, schema: &'a Schema, index_name: &str, kind: &IndexScanKind) -> DbResult<Self> {
        let btree = storage
            .index_btree(index_name)
            .unwrap_or_else(|| unreachable!("optimizeが選んだ索引'{index_name}'はStorageに必ず存在する"));
        let source = match kind {
            IndexScanKind::Point(value) => IndexScanSource::Point(btree.lookup(value)?.into_iter()),
            IndexScanKind::Range { lower, upper } => IndexScanSource::Range(btree.range(lower.as_ref(), upper.as_ref())?),
        };
        Ok(IndexScanExec { storage, table_id, schema, source })
    }
}

impl<'a> Executor for IndexScanExec<'a> {
    fn output_schema(&self) -> &Schema {
        self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            let rid = match &mut self.source {
                IndexScanSource::Point(iter) => match iter.next() {
                    Some(rid) => rid,
                    None => return Ok(None),
                },
                IndexScanSource::Range(iter) => match iter.next() {
                    Some(Ok((_, rid))) => rid,
                    Some(Err(err)) => return Err(err),
                    None => return Ok(None),
                },
            };
            if let Some(bytes) = self.storage.get(self.table_id, rid)? {
                return decode_tuple(self.schema, &bytes).map(Some);
            }
            // Lazy Delete済み(索引にエントリが残っているのにHeapから
            // すでに消えている)場合はここに来る。モジュールドキュメントの
            // とおり読み飛ばして次の`RecordId`へ進む。
        }
    }
}

/// Filter演算子。子の`next()`を、`predicate`が`TRUE`になる行が見つかるまで
/// 繰り返し呼ぶ。一致しなかった行は`kept`のような`Vec`に貯めることなく、
/// その場で捨てて次の子の行へ進む。第18章までの`executor::filter`は子の
/// 結果`Vec<Tuple>`全体を受け取ってから絞り込んだ新しい`Vec<Tuple>`を返して
/// いたが、こちらは呼び出し側が`next()`を1回呼ぶたびに、子の`next()`を
/// 「次に一致する行が見つかるまで」の回数だけ呼ぶ。子を何度呼ぶかは
/// 呼び出し側からは見えないが、`Filter`自身が新しい`Vec`を確保することは無い。
pub struct FilterExec<'a> {
    input: Box<dyn Executor + 'a>,
    predicate: &'a BoundExpr,
    functions: &'a FunctionRegistry,
    schema: Schema,
}

impl<'a> FilterExec<'a> {
    pub fn new(input: Box<dyn Executor + 'a>, predicate: &'a BoundExpr, functions: &'a FunctionRegistry) -> Self {
        let schema = input.output_schema().clone();
        FilterExec { input, predicate, functions, schema }
    }
}

impl<'a> Executor for FilterExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            let Some(tuple) = self.input.next()? else {
                return Ok(None);
            };
            let row = Row::new(&self.schema, &tuple);
            let value = eval_bound_expr(self.predicate, self.functions, Some(&row))?;
            if predicate_matches(value)? {
                return Ok(Some(tuple));
            }
        }
    }
}

/// Projection演算子。子から受け取った1行に`projection`を適用し、そのまま
/// 1行を返す。第18章までの`executor::project`は子の結果`Vec<Tuple>`全体を
/// 受け取ってから出力用の`Vec<Tuple>`をまとめて組み立てていたが、こちらは
/// `next()`が呼ばれるたびに子から1行だけ引き、その1行だけを射影する。
pub struct ProjectionExec<'a> {
    input: Box<dyn Executor + 'a>,
    projection: &'a [BoundSelectItem],
    functions: &'a FunctionRegistry,
    input_schema: Schema,
    out_schema: Schema,
}

impl<'a> ProjectionExec<'a> {
    pub fn new(input: Box<dyn Executor + 'a>, projection: &'a [BoundSelectItem], functions: &'a FunctionRegistry) -> Self {
        let input_schema = input.output_schema().clone();
        let out_schema = logical_plan::projection_schema(&input_schema, projection);
        ProjectionExec { input, projection, functions, input_schema, out_schema }
    }
}

impl<'a> Executor for ProjectionExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.out_schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        let Some(tuple) = self.input.next()? else {
            return Ok(None);
        };
        let row = Row::new(&self.input_schema, &tuple);
        let mut values = Vec::with_capacity(self.projection.len());
        for item in self.projection {
            values.push(eval_bound_expr(&item.expr, self.functions, Some(&row))?);
        }
        Tuple::new(&self.out_schema, values).map(Some)
    }
}

/// `left`と`right`の1行ずつを連結し、`schema`(`logical_plan::join_schema`と
/// 同じ規則)に対する`Tuple`にする(第22章)。`NestedLoopJoinExec`・
/// `HashJoinExec`の両方が同じ形の連結を必要とするため、共通の関数として
/// 切り出してある。
fn concat_tuple(schema: &Schema, left: &Tuple, right: &Tuple) -> DbResult<Tuple> {
    let mut values = Vec::with_capacity(left.values().len() + right.values().len());
    values.extend(left.values().iter().cloned());
    values.extend(right.values().iter().cloned());
    Tuple::new(schema, values)
}

/// Nested Loop Join演算子(第22章)。`ON`が任意の条件でも動く基準実装で、
/// `left`の行1件ごとに`right`の全行を突き合わせる。
///
/// 教科書的なNested Loop Joinは`left`の行1件ごとに`right`の子計画を
/// もう一度実行し直す(rescan)が、この章の`Executor`は`Box<dyn Executor>`と
/// いうTrait Objectであり、状態を持つイテレータを「巻き戻す」手段を
/// 持たない(第19章)。この実装は`right`をコンストラクタで一度だけ`Vec<Tuple>`
/// へ読み切り(build段階、`right`だけをblockingに読む)、以後は`left`から
/// 1行受け取るたびにその`Vec`を先頭から順に見比べる。比較の回数は
/// `left`の行数×`right`の行数のままなので、計算量(O(n×m))は教科書的な
/// 実装と変わらない。
pub struct NestedLoopJoinExec<'a> {
    left: Box<dyn Executor + 'a>,
    right_rows: Vec<Tuple>,
    condition: &'a BoundExpr,
    functions: &'a FunctionRegistry,
    schema: Schema,
    current_left: Option<Tuple>,
    right_index: usize,
}

impl<'a> NestedLoopJoinExec<'a> {
    pub fn new(
        left: Box<dyn Executor + 'a>,
        mut right: Box<dyn Executor + 'a>,
        condition: &'a BoundExpr,
        functions: &'a FunctionRegistry,
    ) -> DbResult<Self> {
        let left_schema = left.output_schema().clone();
        let right_schema = right.output_schema().clone();
        let mut right_rows = Vec::new();
        while let Some(tuple) = right.next()? {
            right_rows.push(tuple);
        }
        let schema = logical_plan::join_schema(&left_schema, &right_schema);
        Ok(NestedLoopJoinExec { left, right_rows, condition, functions, schema, current_left: None, right_index: 0 })
    }
}

impl<'a> Executor for NestedLoopJoinExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            if self.current_left.is_none() {
                let Some(tuple) = self.left.next()? else {
                    return Ok(None);
                };
                self.current_left = Some(tuple);
                self.right_index = 0;
            }
            let left_tuple = self.current_left.as_ref().expect("直前にSomeを設定済み");

            while self.right_index < self.right_rows.len() {
                let right_tuple = &self.right_rows[self.right_index];
                self.right_index += 1;
                let combined = concat_tuple(&self.schema, left_tuple, right_tuple)?;
                let row = Row::new(&self.schema, &combined);
                let value = eval_bound_expr(self.condition, self.functions, Some(&row))?;
                if predicate_matches(value)? {
                    return Ok(Some(combined));
                }
            }
            // `right_rows`を使い切った。この`left`の行についてはこれ以上
            // 一致しないので、次の`left`の行へ進む。
            self.current_left = None;
        }
    }
}

/// Hash Join演算子(第22章)。`ON`が等値条件の連言(AND)であるときに選ばれる
/// (`physical_plan::optimize`の`split_equi_join_keys`)。
///
/// **Build側**(`right`)を先にコンストラクタで全件読み切り、`right_keys`を
/// 評価した結果をハッシュテーブルの鍵にして`Vec<Tuple>`(同じ鍵を持つ行の
/// 多重集合)を溜める。**Probe側**(`left`)は`next()`が呼ばれるたびに1行ずつ
/// 引き、`left_keys`を評価した鍵でハッシュテーブルを引いて一致する`right`の
/// 行と連結する。Build段階だけがblockingで、Probe段階は`FilterExec`と同じ
/// 「一致するまで子を引く」streamingの形になる。
///
/// **NULLキーは結合しない**(SQLの等価比較は`NULL = NULL`をUNKNOWNとみなす、
/// 第8章の三値論理)。Build側は鍵にNULLを含む行をハッシュテーブルへ挿入
/// しない(挿入しなければ、その行はどのProbe行とも一致しようがなく、結果的に
/// 結合結果から自然に除外される)。Probe側も鍵にNULLを含む行はハッシュ
/// テーブルを引かずに次の行へ進む。`Vec<Value>`をそのままキーにした場合、
/// 導出された`Hash`/`Eq`は`NULL`同士を構造的に等しいとみなしてしまう
/// (第21章の`GROUP BY`はこれを利用してNULL同士を同じグループにまとめていた)
/// ため、この章のJOINではその判定に頼らず、鍵を計算した時点で明示的に
/// NULLを検査する。
pub struct HashJoinExec<'a> {
    left: Box<dyn Executor + 'a>,
    left_schema: Schema,
    build: HashMap<Vec<Value>, Vec<Tuple>>,
    /// `(left_key, right_key)`の対の並び。Build段階で使い終えた`right_key`も
    /// そのまま保持しているのは、`keys`という1つの借用を丸ごと持ち回すほうが、
    /// `left_key`だけを複製して別のフィールドに分ける(所有権とライフタイム
    /// が余分に絡む)よりも単純だからである。
    keys: &'a [(BoundExpr, BoundExpr)],
    functions: &'a FunctionRegistry,
    schema: Schema,
    current_left: Option<Tuple>,
    current_key: Option<Vec<Value>>,
    match_index: usize,
}

impl<'a> HashJoinExec<'a> {
    /// `keys`は`(left_key, right_key)`の対の並びで、`right_key`はすでに
    /// `right`単体のスキーマ上のローカルな添字へシフト済みでなければならない
    /// (`HashJoinNode::keys`のドキュメント参照)。
    pub fn new(
        left: Box<dyn Executor + 'a>,
        mut right: Box<dyn Executor + 'a>,
        keys: &'a [(BoundExpr, BoundExpr)],
        functions: &'a FunctionRegistry,
    ) -> DbResult<Self> {
        let left_schema = left.output_schema().clone();
        let right_schema = right.output_schema().clone();
        let schema = logical_plan::join_schema(&left_schema, &right_schema);

        let mut build: HashMap<Vec<Value>, Vec<Tuple>> = HashMap::new();
        while let Some(tuple) = right.next()? {
            let row = Row::new(&right_schema, &tuple);
            let key: Vec<Value> =
                keys.iter().map(|(_, right_key)| eval_bound_expr(right_key, functions, Some(&row))).collect::<DbResult<_>>()?;
            if key.iter().any(Value::is_null) {
                continue; // NULLキーは結合しない(モジュールのドキュメント参照)
            }
            build.entry(key).or_default().push(tuple);
        }

        Ok(HashJoinExec { left, left_schema, build, keys, functions, schema, current_left: None, current_key: None, match_index: 0 })
    }
}

impl<'a> Executor for HashJoinExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            if self.current_left.is_none() {
                let Some(tuple) = self.left.next()? else {
                    return Ok(None);
                };
                let row = Row::new(&self.left_schema, &tuple);
                let key: Vec<Value> = self
                    .keys
                    .iter()
                    .map(|(left_key, _)| eval_bound_expr(left_key, self.functions, Some(&row)))
                    .collect::<DbResult<_>>()?;
                self.current_left = Some(tuple);
                if key.iter().any(Value::is_null) {
                    self.current_key = None; // NULLキーは結合しない
                } else {
                    self.current_key = Some(key);
                }
                self.match_index = 0;
            }

            let Some(key) = &self.current_key else {
                self.current_left = None;
                continue;
            };
            let matches = self.build.get(key);
            let found = matches.and_then(|rows| rows.get(self.match_index));
            match found {
                Some(right_tuple) => {
                    self.match_index += 1;
                    let left_tuple = self.current_left.as_ref().expect("直前にSomeを設定済み");
                    return concat_tuple(&self.schema, left_tuple, right_tuple).map(Some);
                }
                None => {
                    self.current_left = None;
                }
            }
        }
    }
}

/// Index Nested Loop Join演算子(第25章)。`left`の行1件ごとに`outer_key`を
/// 評価し、その値で`index_name`が指す索引を`lookup`して内側テーブルの
/// `RecordId`を求め、Heapから`fetch`して連結する。
///
/// `NestedLoopJoinExec`・`HashJoinExec`と違い、内側テーブルの全行を
/// コンストラクタで読み切ることも、`Vec`やハッシュテーブルへ溜めることも
/// しない。内側の行は、外側の1行が来るたびに索引への`lookup`(`O(log n)`)で
/// その都度求める。外側の行数を`n`、一致1件あたりの索引探索を`O(log m)`
/// (`m`は内側テーブルの行数)とすると、総コストは`O(n log m)`になり、
/// `NestedLoopJoinExec`の`O(n × m)`より内側テーブルが大きいほど有利になる
/// (本文の実測を参照)。
///
/// `outer_key`が`NULL`を評価した場合は、索引を引かずに一致0件として扱う
/// (`HashJoinExec`のNULLキー除外と同じ、SQLの三値論理にもとづく規則)。
pub struct IndexNestedLoopJoinExec<'a> {
    left: Box<dyn Executor + 'a>,
    storage: &'a Storage,
    table_id: TableId,
    index_name: &'a str,
    outer_key: &'a BoundExpr,
    functions: &'a FunctionRegistry,
    left_schema: Schema,
    right_schema: Schema,
    schema: Schema,
    current_left: Option<Tuple>,
    matches: std::vec::IntoIter<RecordId>,
}

impl<'a> IndexNestedLoopJoinExec<'a> {
    pub fn new(
        left: Box<dyn Executor + 'a>,
        storage: &'a Storage,
        table_id: TableId,
        right_schema: Schema,
        index_name: &'a str,
        outer_key: &'a BoundExpr,
        functions: &'a FunctionRegistry,
    ) -> Self {
        let left_schema = left.output_schema().clone();
        let schema = logical_plan::join_schema(&left_schema, &right_schema);
        IndexNestedLoopJoinExec {
            left,
            storage,
            table_id,
            index_name,
            outer_key,
            functions,
            left_schema,
            right_schema,
            schema,
            current_left: None,
            matches: Vec::new().into_iter(),
        }
    }
}

impl<'a> Executor for IndexNestedLoopJoinExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            if let Some(rid) = self.matches.next() {
                let left_tuple = self.current_left.as_ref().expect("直前にSomeを設定済み");
                if let Some(bytes) = self.storage.get(self.table_id, rid)? {
                    let right_tuple = decode_tuple(&self.right_schema, &bytes)?;
                    return concat_tuple(&self.schema, left_tuple, &right_tuple).map(Some);
                }
                // Lazy Delete済み(IndexScanExecのドキュメントを参照)。
                // この`rid`は読み飛ばし、`matches`の続きへ進む。
                continue;
            }

            let Some(tuple) = self.left.next()? else {
                return Ok(None);
            };
            let row = Row::new(&self.left_schema, &tuple);
            let key = eval_bound_expr(self.outer_key, self.functions, Some(&row))?;
            self.current_left = Some(tuple);
            self.matches = if key.is_null() {
                Vec::new().into_iter() // NULLキーは結合しない
            } else {
                let btree = self
                    .storage
                    .index_btree(self.index_name)
                    .unwrap_or_else(|| unreachable!("optimizeが選んだ索引'{}'はStorageに必ず存在する", self.index_name));
                btree.lookup(&key)?.into_iter()
            };
        }
    }
}

/// グループ1個ぶんの集約状態(第21章)。
///
/// `count`・`sum`・`extreme`をすべて持つが、実際に使うのは`calls[i].func`が
/// どの集約関数かによって1つだけである(`AggregateCall`ごとに1個の`AggState`を
/// 持つ設計にしたのは、`COUNT`・`SUM`・`MIN`・`MAX`を1つの`enum`に分けるより、
/// 「使わないフィールドは初期値のまま」という単純な形のほうがこの章の分量に
/// 見合っているため)。
struct AggState {
    /// `COUNT`が使う。`COUNT(*)`は全行を、`COUNT(expr)`は`expr`が`NULL`でない
    /// 行だけを数える。
    count: i64,
    /// `SUM`が使う。`None`はまだ非`NULL`の値を1件も見ていないことを表し、
    /// 最終結果は`NULL`になる(空グループ、または全行が`NULL`だったグループの
    /// `SUM`は`NULL`というSQL標準の規則)。
    sum: Option<i64>,
    /// `MIN`・`MAX`が使う。`None`の意味は`sum`と同じ。
    extreme: Option<Value>,
}

impl AggState {
    fn new() -> Self {
        AggState { count: 0, sum: None, extreme: None }
    }

    /// この行の評価値(`COUNT(*)`なら`None`、それ以外は`arg`を評価した値)で
    /// 状態を更新する。
    fn update(&mut self, func: AggregateFunc, value: Option<&Value>) -> DbResult<()> {
        match func {
            AggregateFunc::Count => {
                let counts = value.is_none_or(|v| !v.is_null());
                if counts {
                    self.count += 1;
                }
            }
            AggregateFunc::Sum => {
                let Some(value) = value else { unreachable!("Binderが検査済み: SUMは必ずargを持つ") };
                if !value.is_null() {
                    let Value::BigInt(n) = value else { unreachable!("Binderが検査済み: SUMの引数はBIGINT") };
                    let base = self.sum.unwrap_or(0);
                    self.sum = Some(base.checked_add(*n).ok_or_else(|| DbError::Eval(format!("整数オーバーフロー: SUM({n})")))?);
                }
            }
            AggregateFunc::Min | AggregateFunc::Max => {
                let Some(value) = value else { unreachable!("Binderが検査済み: MIN/MAXは必ずargを持つ") };
                if !value.is_null() {
                    let better = match &self.extreme {
                        None => true,
                        Some(current) => {
                            let ordering = compare_values(value, current);
                            match func {
                                AggregateFunc::Min => ordering == Ordering::Less,
                                AggregateFunc::Max => ordering == Ordering::Greater,
                                _ => unreachable!("この分岐はMIN/MAXでのみ到達する"),
                            }
                        }
                    };
                    if better {
                        self.extreme = Some(value.clone());
                    }
                }
            }
        }
        Ok(())
    }

    fn finish(&self, func: AggregateFunc) -> Value {
        match func {
            AggregateFunc::Count => Value::BigInt(self.count),
            AggregateFunc::Sum => self.sum.map(Value::BigInt).unwrap_or(Value::Null),
            AggregateFunc::Min | AggregateFunc::Max => self.extreme.clone().unwrap_or(Value::Null),
        }
    }
}

/// Hash Aggregate演算子(第21章)。`GROUP BY`のグループ化キー(`Vec<Value>`)を
/// ハッシュテーブルの鍵にして、行を1件読むたびに該当するグループの集約状態
/// (`AggState`)を更新する。
///
/// **blocking演算子**である。子の行を1件返しただけではどのグループに属する
/// 集約結果も確定しないため(最後の1行を読むまで、どのグループの`COUNT`が
/// 最終的にいくつになるか分からない)、`next()`が1回でも呼ばれた時点で子を
/// `None`が返るまで読み切ってから、初めて1件目の結果を返す。`FilterExec`・
/// `ProjectionExec`(第19章)が体現していた「子から1行受け取ったら即座に1行
/// 返す」というPull型実行の性質を、この演算子は持たない。この非対称性は
/// 実装の妥協ではなく、集約という演算自体の性質である(`SortExec`も同じ理由で
/// blockingになる。本文の解説を参照)。
///
/// `GROUP BY`が無いグループ化キーが空の集約(`SELECT COUNT(*) FROM t`)では、
/// 子の行が1件も無くてもちょうど1行を返す(空のテーブルに対する`COUNT(*)`が
/// `0`を返すのはこのため)。`GROUP BY`があるのに子の行が1件も無い場合は、
/// グループそのものが1つも存在しないため0行を返す。
pub struct HashAggregateExec {
    schema: Schema,
    rows: std::vec::IntoIter<Tuple>,
}

impl HashAggregateExec {
    pub fn new(
        mut input: Box<dyn Executor + '_>,
        group_by: &[BoundExpr],
        calls: &[AggregateCall],
        schema: Schema,
        functions: &FunctionRegistry,
    ) -> DbResult<Self> {
        let input_schema = input.output_schema().clone();
        // グループの出力順序を、HashMapの走査順(非決定的)ではなく、そのグループの
        // キーが最初に現れた行の順序にするため、鍵→`groups`上の添字を持つ
        // `HashMap`と、状態そのものを持つ`Vec`を分けて持つ。golden/differential
        // テストの出力を安定させる(`ORDER BY`が無い集約結果の順序はSQLの意味論
        // 上未規定だが、未規定だからといって実行のたびに変わってよい理由には
        // ならない)。
        let mut order: HashMap<Vec<Value>, usize> = HashMap::new();
        let mut groups: Vec<(Vec<Value>, Vec<AggState>)> = Vec::new();

        while let Some(tuple) = input.next()? {
            let row = Row::new(&input_schema, &tuple);
            let key: Vec<Value> =
                group_by.iter().map(|expr| eval_bound_expr(expr, functions, Some(&row))).collect::<DbResult<Vec<_>>>()?;

            let index = *order.entry(key.clone()).or_insert_with(|| {
                groups.push((key, calls.iter().map(|_| AggState::new()).collect()));
                groups.len() - 1
            });

            for (call, state) in calls.iter().zip(groups[index].1.iter_mut()) {
                let value = match &call.arg {
                    Some(arg) => Some(eval_bound_expr(arg, functions, Some(&row))?),
                    None => None,
                };
                state.update(call.func, value.as_ref())?;
            }
        }

        if groups.is_empty() && group_by.is_empty() {
            groups.push((Vec::new(), calls.iter().map(|_| AggState::new()).collect()));
        }

        let rows = groups
            .into_iter()
            .map(|(key, states)| {
                let mut values = key;
                for (call, state) in calls.iter().zip(states.iter()) {
                    values.push(state.finish(call.func));
                }
                Tuple::new(&schema, values)
            })
            .collect::<DbResult<Vec<_>>>()?;

        Ok(HashAggregateExec { schema, rows: rows.into_iter() })
    }
}

impl Executor for HashAggregateExec {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        Ok(self.rows.next())
    }
}

/// Distinct演算子(第21章)。すでに返した行の集合(`seen`)をハッシュ集合で
/// 持ち、子から引いた行がまだ`seen`に無ければそのまま返し、あれば読み飛ばして
/// 次の行へ進む。
///
/// `HashAggregateExec`・`SortExec`とは異なり、子を`None`まで読み切ってから
/// 初めて1行返すわけではない。子の最初の行は(重複の判定対象がまだ無いので)
/// 必ずそのまま返され、`next()`は`FilterExec`と同じ「一致するまで子を引く」
/// 形になる。ただし`seen`というハッシュ集合はこれまでに返した行数に比例して
/// 育ち続け、`Filter`のように読み捨てた行を完全に忘れるわけではない。
/// 「1行も返さないうちに全件読み切る」わけではないという意味では
/// streamingだが、メモリ使用量は`Filter`のようには有界にならない。この
/// 中間的な性質を、本文では「blocking/streamingの二分法では割り切れない
/// 演算子」として扱う。
///
/// `NULL`を含む行同士も、値がすべて等しければ重複とみなす(`DISTINCT`は
/// `GROUP BY`と同じく`NULL`同士を同じグループとして扱うのがSQL標準の規則で
/// あり、第20章の`UNIQUE`制約が`NULL`同士を重複とみなさなかったのとは対照的
/// である。`UNIQUE`が守るのは「値が分かっていて、かつ重なっている」ことの
/// 禁止だが、`DISTINCT`はそもそも値が分かっているかどうかを問わず、行の
/// 見た目が同じかどうかだけを見る)。
pub struct DistinctExec<'a> {
    input: Box<dyn Executor + 'a>,
    seen: HashSet<Vec<Value>>,
    schema: Schema,
}

impl<'a> DistinctExec<'a> {
    pub fn new(input: Box<dyn Executor + 'a>) -> Self {
        let schema = input.output_schema().clone();
        DistinctExec { input, seen: HashSet::new(), schema }
    }
}

impl<'a> Executor for DistinctExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            let Some(tuple) = self.input.next()? else {
                return Ok(None);
            };
            if self.seen.insert(tuple.values().to_vec()) {
                return Ok(Some(tuple));
            }
        }
    }
}

/// Sort演算子(第21章)。In-memory Sortのみを実装する。子を`None`まで読み切って
/// `Vec<Tuple>`へ溜めてから並べ替え、以後は溜めた`Vec`を先頭から返す
/// **blocking演算子**である。
///
/// 外部ソート(メモリに載り切らないほど大きい入力を、一時ファイルへの
/// スピルを挟んで並べ替える手法)は、このクレートが対象にする規模(教材の
/// テストデータ)を超える発展的な話題であり、本編では扱わない(章末の
/// 演習課題を参照)。
///
/// 並べ替えは[`Vec::sort_by`]を使う。これは安定ソート(要素の元の順序を、
/// 比較結果が等しい要素同士については保つ)であり、`ORDER BY`のキーが完全に
/// 一致する複数行の相対順序は、`Sort`に入力される前の順序(この章の実装では
/// `Scan`が返す挿入順)のまま保たれる。
///
/// `NULL`の順序は、[`compare_values`]が定義する全順序(`NULL`をどの値よりも
/// 小さいとみなす)を`ASC`ならそのまま、`DESC`なら反転させて使う。結果として
/// `ASC`では`NULL`が先頭に、`DESC`では末尾に来る(SQLiteの既定の並び順と
/// 一致させてある。本文の解説を参照)。
pub struct SortExec {
    schema: Schema,
    rows: std::vec::IntoIter<Tuple>,
}

impl SortExec {
    pub fn new(mut input: Box<dyn Executor + '_>, keys: &[SortKey], functions: &FunctionRegistry) -> DbResult<Self> {
        let schema = input.output_schema().clone();

        let mut keyed: Vec<(Vec<Value>, Tuple)> = Vec::new();
        while let Some(tuple) = input.next()? {
            let row = Row::new(&schema, &tuple);
            let key: Vec<Value> =
                keys.iter().map(|k| eval_bound_expr(&k.expr, functions, Some(&row))).collect::<DbResult<Vec<_>>>()?;
            keyed.push((key, tuple));
        }

        keyed.sort_by(|(a, _), (b, _)| {
            for (i, key) in keys.iter().enumerate() {
                let ordering = compare_values(&a[i], &b[i]);
                let ordering = if key.desc { ordering.reverse() } else { ordering };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        });

        let rows: Vec<Tuple> = keyed.into_iter().map(|(_, tuple)| tuple).collect();
        Ok(SortExec { schema, rows: rows.into_iter() })
    }
}

impl Executor for SortExec {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        Ok(self.rows.next())
    }
}

/// Limit演算子(第21章)。`OFFSET`件を読み捨ててから、`LIMIT`件を返したら
/// それ以上子の`next()`を呼ばない、streaming演算子である。
///
/// `HashAggregateExec`・`SortExec`が子を`None`まで読み切ってから1行目を返す
/// のとは対照的に、`LimitExec`は要求された件数を返し終えた時点で子への
/// 問い合わせそのものを止める。`SELECT * FROM t ORDER BY id LIMIT 1`のような
/// 文では、`Sort`が全件を読み切って並べ替える必要があるため全体としては
/// blockingな計画のままだが、`SELECT * FROM t LIMIT 1`のように`Sort`を伴わない
/// 場合は、`SeqScan`が2行目以降を1行も読まずに済む(章末の演習課題で、この
/// 違いを実際に確認する)。
pub struct LimitExec<'a> {
    input: Box<dyn Executor + 'a>,
    remaining_offset: usize,
    remaining_limit: Option<usize>,
    schema: Schema,
}

impl<'a> LimitExec<'a> {
    pub fn new(input: Box<dyn Executor + 'a>, limit: Option<usize>, offset: Option<usize>) -> Self {
        let schema = input.output_schema().clone();
        LimitExec { input, remaining_offset: offset.unwrap_or(0), remaining_limit: limit, schema }
    }
}

impl<'a> Executor for LimitExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        if self.remaining_limit == Some(0) {
            return Ok(None);
        }
        while self.remaining_offset > 0 {
            self.remaining_offset -= 1;
            if self.input.next()?.is_none() {
                return Ok(None);
            }
        }
        let Some(tuple) = self.input.next()? else {
            return Ok(None);
        };
        if let Some(remaining) = &mut self.remaining_limit {
            *remaining -= 1;
        }
        Ok(Some(tuple))
    }
}

/// `EXPLAIN ANALYZE`(第27章)が実測行数を集めるための、`Executor`1個の
/// ラッパー。`inner.next()`が`Some`を返すたびに`count`を1つ増やす。
///
/// カウンタを`Rc<Cell<u64>>`で持つのは、`Database::build_query_executor`が
/// `PhysicalPlan`の木をたどりながら`Box<dyn Executor>`の木を組み立てたあと、
/// 呼び出し側(`Database::execute_explain`)が`Executor`の木とは別に
/// [`CounterNode`]の木を保持し続け、実行が終わってから(=`Box<dyn Executor>`の
/// 所有権が尽きたあとで)各カウンタの値を読む必要があるためである。
/// `&mut u64`のような通常の参照では、`Executor`の木を借用したまま結果を
/// 読み出すことになり借用規則に反する。
pub struct CountingExec<'a> {
    inner: Box<dyn Executor + 'a>,
    count: Rc<Cell<u64>>,
}

impl<'a> CountingExec<'a> {
    pub fn new(inner: Box<dyn Executor + 'a>, count: Rc<Cell<u64>>) -> Self {
        CountingExec { inner, count }
    }
}

impl<'a> Executor for CountingExec<'a> {
    fn output_schema(&self) -> &Schema {
        self.inner.output_schema()
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        let result = self.inner.next()?;
        if result.is_some() {
            self.count.set(self.count.get() + 1);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::{Binder, BoundStatement};
    use crate::catalog::Catalog;
    use crate::logical_plan::build_select;
    use crate::parser::parse_statement;
    use crate::types::{Column, DataType, Value};
    use std::cell::Cell;
    use std::rc::Rc;

    fn users_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                "users",
                Schema::new(vec![
                    Column::new("id", DataType::BigInt, false),
                    Column::new("name", DataType::Text, true),
                ]),
            )
            .unwrap();
        catalog
    }

    fn bind_select(sql: &str) -> crate::binder::BoundSelect {
        let catalog = users_catalog();
        let functions = FunctionRegistry::with_builtins();
        let statement = parse_statement(sql).unwrap();
        match Binder::new(&catalog, &functions, sql).bind(statement).unwrap() {
            BoundStatement::Select(select) => *select,
            other => panic!("Selectを期待したが{other:?}が返った"),
        }
    }

    // ---- optimize/Display ----

    #[test]
    fn optimize_turns_scan_into_seq_scan() {
        let select = bind_select("SELECT name FROM users WHERE id = 42");
        let physical = optimize(build_select(select), None, &NoStats);
        assert_eq!(physical.to_string(), "Projection(name)\n  └─ Filter(id = 42)\n    └─ SeqScan(users)\n");
    }

    #[test]
    fn optimize_preserves_output_schema() {
        let select = bind_select("SELECT id, name FROM users");
        let physical = optimize(build_select(select), None, &NoStats);
        let schema = physical.output_schema();
        let names: Vec<&str> = schema.columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name"]);
    }

    // ---- Executor: 手作りの行(CountingExecutor)を使った合成テスト ----

    fn users_schema() -> Schema {
        Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("name", DataType::Text, true)])
    }

    fn tuple(id: i64, name: &str) -> Tuple {
        let schema = users_schema();
        Tuple::new(&schema, vec![Value::BigInt(id), Value::Text(name.to_string())]).unwrap()
    }

    /// テスト専用の葉演算子。`next()`が呼ばれるたびに`pulled`をインクリメント
    /// してから次の行を返す。テストコードから「子が実際に何回`next()`を
    /// 呼ばれたか」を数えるために使う。
    struct CountingExecutor {
        schema: Schema,
        rows: std::vec::IntoIter<Tuple>,
        pulled: Rc<Cell<usize>>,
    }

    impl Executor for CountingExecutor {
        fn output_schema(&self) -> &Schema {
            &self.schema
        }

        fn next(&mut self) -> DbResult<Option<Tuple>> {
            self.pulled.set(self.pulled.get() + 1);
            Ok(self.rows.next())
        }
    }

    fn bound_true_predicate() -> BoundExpr {
        let select = bind_select("SELECT id FROM users WHERE id = id");
        select.predicate.unwrap()
    }

    fn bound_id_projection() -> Vec<BoundSelectItem> {
        bind_select("SELECT id FROM users").projection
    }

    #[test]
    fn filter_pulls_from_child_lazily_row_by_row() {
        // 1,000行のうち全行が`predicate`(`id = id`、常にTRUE)に一致する。
        // それでも根の`Projection`から3行しか引かなければ、子の葉からも
        // ちょうど3回しか`next()`が呼ばれない。第18章までの`executor::filter`
        // なら、1,000行すべてを1回の呼び出しで`Vec`にまとめてから返していた。
        let rows: Vec<Tuple> = (0..1000).map(|i| tuple(i, "x")).collect();
        let pulled = Rc::new(Cell::new(0));
        let leaf = CountingExecutor { schema: users_schema(), rows: rows.into_iter(), pulled: pulled.clone() };

        let predicate = bound_true_predicate();
        let functions = FunctionRegistry::with_builtins();
        let mut filter = FilterExec::new(Box::new(leaf), &predicate, &functions);

        filter.next().unwrap();
        filter.next().unwrap();
        filter.next().unwrap();

        assert_eq!(pulled.get(), 3);
    }

    #[test]
    fn scan_filter_projection_pipeline_pulls_exactly_as_many_rows_as_requested() {
        // Scan相当のCountingExecutor→Filter→Projectionという3段の合成でも、
        // 根から3回`next()`を呼んだだけなら、葉は3回しか`next()`されない。
        let rows: Vec<Tuple> = (0..1000).map(|i| tuple(i, "x")).collect();
        let pulled = Rc::new(Cell::new(0));
        let leaf = CountingExecutor { schema: users_schema(), rows: rows.into_iter(), pulled: pulled.clone() };

        let predicate = bound_true_predicate();
        let projection = bound_id_projection();
        let functions = FunctionRegistry::with_builtins();
        let filter = FilterExec::new(Box::new(leaf), &predicate, &functions);
        let mut projection_exec = ProjectionExec::new(Box::new(filter), &projection, &functions);

        for _ in 0..3 {
            projection_exec.next().unwrap();
        }

        assert_eq!(pulled.get(), 3);
    }

    #[test]
    fn filter_skips_non_matching_rows_without_buffering() {
        let rows = vec![tuple(1, "Alice"), tuple(2, "Bob"), tuple(3, "Carol")];
        let pulled = Rc::new(Cell::new(0));
        let leaf = CountingExecutor { schema: users_schema(), rows: rows.into_iter(), pulled: pulled.clone() };

        let predicate_select = bind_select("SELECT id FROM users WHERE id = 3");
        let predicate = predicate_select.predicate.unwrap();
        let functions = FunctionRegistry::with_builtins();
        let mut filter = FilterExec::new(Box::new(leaf), &predicate, &functions);

        let matched = filter.next().unwrap().unwrap();
        assert_eq!(matched.values()[0], Value::BigInt(3));
        // `id = 3`に一致するまでに3行(1・2・3)ぶん子から引いている。
        assert_eq!(pulled.get(), 3);
        assert!(filter.next().unwrap().is_none());
    }

    #[test]
    fn values_exec_evaluates_all_rows_eagerly_at_construction() {
        let schema = Schema::new(vec![Column::new("x", DataType::BigInt, false)]);
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![vec![Expr::IntLiteral { value: 1, span: crate::lexer::Span { start: 0, end: 0 } }]];
        let mut values = ValuesExec::new(schema, &functions, &rows).unwrap();
        assert_eq!(values.next().unwrap().unwrap().values()[0], Value::BigInt(1));
        assert!(values.next().unwrap().is_none());
    }

    #[test]
    fn mem_seq_scan_exec_clones_rows_one_at_a_time() {
        let mut table = MemTable::new();
        table.rows_mut().push(tuple(1, "Alice"));
        table.rows_mut().push(tuple(2, "Bob"));
        let schema = users_schema();

        let mut scan = MemSeqScanExec::new(&schema, &table);
        assert_eq!(scan.next().unwrap().unwrap().values()[0], Value::BigInt(1));
        assert_eq!(scan.next().unwrap().unwrap().values()[0], Value::BigInt(2));
        assert!(scan.next().unwrap().is_none());
    }

    // ---- 第21章: Sort / Limit / Distinct / Aggregate ----

    /// `dept`(TEXT、NULL許容)・`amount`(BIGINT、NULL許容)を持つカタログ。
    /// `users`(`id`が`NOT NULL`)ではSUM/MIN/MAXのNULL規則を確かめられないため、
    /// この章のテスト専用に別のカタログを用意する。
    fn orders_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table(
                "orders",
                Schema::new(vec![
                    Column::new("dept", DataType::Text, true),
                    Column::new("amount", DataType::BigInt, true),
                ]),
            )
            .unwrap();
        catalog
    }

    fn bind_select_orders(sql: &str) -> crate::binder::BoundSelect {
        let catalog = orders_catalog();
        let functions = FunctionRegistry::with_builtins();
        let statement = parse_statement(sql).unwrap();
        match Binder::new(&catalog, &functions, sql).bind(statement).unwrap() {
            BoundStatement::Select(select) => *select,
            other => panic!("Selectを期待したが{other:?}が返った"),
        }
    }

    fn orders_schema() -> Schema {
        Schema::new(vec![Column::new("dept", DataType::Text, true), Column::new("amount", DataType::BigInt, true)])
    }

    fn order_row(dept: Option<&str>, amount: Option<i64>) -> Tuple {
        let schema = orders_schema();
        let dept = dept.map(|s| Value::Text(s.to_string())).unwrap_or(Value::Null);
        let amount = amount.map(Value::BigInt).unwrap_or(Value::Null);
        Tuple::new(&schema, vec![dept, amount]).unwrap()
    }

    fn exec_over_rows(rows: Vec<Tuple>) -> Box<dyn Executor> {
        Box::new(ValuesRowsExec { schema: orders_schema(), rows: rows.into_iter() })
    }

    /// 手作りの`Vec<Tuple>`をそのまま流すだけの葉演算子。`ValuesExec`は`Expr`を
    /// 評価する構築時のコストがあるため、すでに`Tuple`を持っているテストでは
    /// この単純な葉のほうが書きやすい。
    struct ValuesRowsExec {
        schema: Schema,
        rows: std::vec::IntoIter<Tuple>,
    }

    impl Executor for ValuesRowsExec {
        fn output_schema(&self) -> &Schema {
            &self.schema
        }
        fn next(&mut self) -> DbResult<Option<Tuple>> {
            Ok(self.rows.next())
        }
    }

    fn collect_all(exec: &mut dyn Executor) -> Vec<Tuple> {
        let mut rows = Vec::new();
        while let Some(tuple) = exec.next().unwrap() {
            rows.push(tuple);
        }
        rows
    }

    // ---- HashAggregateExec ----

    #[test]
    fn count_star_counts_all_rows_including_null_columns() {
        let select = bind_select_orders("SELECT COUNT(*) FROM orders");
        let aggregate = select.aggregate.unwrap();
        let rows = vec![order_row(Some("eng"), Some(1)), order_row(None, None), order_row(Some("eng"), None)];
        let functions = FunctionRegistry::with_builtins();
        let mut exec = HashAggregateExec::new(
            exec_over_rows(rows),
            &aggregate.group_by,
            &aggregate.calls,
            aggregate.schema.clone(),
            &functions,
        )
        .unwrap();
        let result = collect_all(&mut exec);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].values(), &[Value::BigInt(3)]);
    }

    #[test]
    fn count_of_expr_ignores_null_values() {
        let select = bind_select_orders("SELECT COUNT(amount) FROM orders");
        let aggregate = select.aggregate.unwrap();
        let rows = vec![order_row(Some("eng"), Some(1)), order_row(None, None), order_row(Some("eng"), Some(2))];
        let functions = FunctionRegistry::with_builtins();
        let mut exec = HashAggregateExec::new(
            exec_over_rows(rows),
            &aggregate.group_by,
            &aggregate.calls,
            aggregate.schema.clone(),
            &functions,
        )
        .unwrap();
        let result = collect_all(&mut exec);
        assert_eq!(result[0].values(), &[Value::BigInt(2)]);
    }

    #[test]
    fn sum_of_empty_group_is_null_but_count_is_zero() {
        // `GROUP BY`が無い集約は、対象行が0件でもちょうど1行を返す
        // (空のテーブルへの`COUNT(*)`が`0`を返すのと同じ規則)。
        let select = bind_select_orders("SELECT COUNT(*), SUM(amount), MIN(amount), MAX(amount) FROM orders");
        let aggregate = select.aggregate.unwrap();
        let functions = FunctionRegistry::with_builtins();
        let mut exec = HashAggregateExec::new(
            exec_over_rows(Vec::new()),
            &aggregate.group_by,
            &aggregate.calls,
            aggregate.schema.clone(),
            &functions,
        )
        .unwrap();
        let result = collect_all(&mut exec);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].values(), &[Value::BigInt(0), Value::Null, Value::Null, Value::Null]);
    }

    #[test]
    fn sum_ignores_null_and_is_null_when_all_values_are_null() {
        let select = bind_select_orders("SELECT SUM(amount) FROM orders");
        let aggregate = select.aggregate.unwrap();
        let functions = FunctionRegistry::with_builtins();

        let mut exec = HashAggregateExec::new(
            exec_over_rows(vec![order_row(Some("eng"), Some(3)), order_row(Some("eng"), None), order_row(Some("eng"), Some(4))]),
            &aggregate.group_by,
            &aggregate.calls,
            aggregate.schema.clone(),
            &functions,
        )
        .unwrap();
        assert_eq!(collect_all(&mut exec)[0].values(), &[Value::BigInt(7)]);

        let mut exec_all_null = HashAggregateExec::new(
            exec_over_rows(vec![order_row(Some("eng"), None)]),
            &aggregate.group_by,
            &aggregate.calls,
            aggregate.schema.clone(),
            &functions,
        )
        .unwrap();
        assert_eq!(collect_all(&mut exec_all_null)[0].values(), &[Value::Null]);
    }

    #[test]
    fn group_by_groups_null_keys_together() {
        // `GROUP BY`は`NULL`同士を同じグループとみなす(標準SQLの規則。
        // 第20章の`UNIQUE`が`NULL`同士を別扱いしたのとは対照的)。
        let select = bind_select_orders("SELECT dept, COUNT(*) FROM orders GROUP BY dept");
        let aggregate = select.aggregate.unwrap();
        let functions = FunctionRegistry::with_builtins();
        let mut exec = HashAggregateExec::new(
            exec_over_rows(vec![order_row(None, Some(1)), order_row(None, Some(2)), order_row(Some("eng"), Some(3))]),
            &aggregate.group_by,
            &aggregate.calls,
            aggregate.schema.clone(),
            &functions,
        )
        .unwrap();
        let mut result = collect_all(&mut exec);
        result.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        assert_eq!(result.len(), 2);
        assert!(result.contains(&Tuple::new(&aggregate.schema, vec![Value::Null, Value::BigInt(2)]).unwrap()));
        assert!(
            result.contains(&Tuple::new(&aggregate.schema, vec![Value::Text("eng".to_string()), Value::BigInt(1)]).unwrap())
        );
    }

    #[test]
    fn group_by_with_no_input_rows_produces_no_groups() {
        // `GROUP BY`が無い集約(前のテスト)とは対照的に、`GROUP BY`があるのに
        // 対象行が0件なら、グループそのものが1つも存在しないため0行を返す。
        let select = bind_select_orders("SELECT dept, COUNT(*) FROM orders GROUP BY dept");
        let aggregate = select.aggregate.unwrap();
        let functions = FunctionRegistry::with_builtins();
        let mut exec = HashAggregateExec::new(
            exec_over_rows(Vec::new()),
            &aggregate.group_by,
            &aggregate.calls,
            aggregate.schema.clone(),
            &functions,
        )
        .unwrap();
        assert!(collect_all(&mut exec).is_empty());
    }

    #[test]
    fn min_max_ignore_null_values() {
        let select = bind_select_orders("SELECT MIN(amount), MAX(amount) FROM orders");
        let aggregate = select.aggregate.unwrap();
        let functions = FunctionRegistry::with_builtins();
        let mut exec = HashAggregateExec::new(
            exec_over_rows(vec![order_row(None, None), order_row(None, Some(5)), order_row(None, Some(1)), order_row(None, Some(3))]),
            &aggregate.group_by,
            &aggregate.calls,
            aggregate.schema.clone(),
            &functions,
        )
        .unwrap();
        assert_eq!(collect_all(&mut exec)[0].values(), &[Value::BigInt(1), Value::BigInt(5)]);
    }

    // ---- SortExec ----

    /// `Sort`は常に`Projection`の直後(合成順序`Projection → Distinct → Sort`)に
    /// 置かれるため、`ORDER BY`のキーは射影後の出力列(ここでは`amount`1列だけの
    /// `Schema`)を指す。この`Schema`に対応する行を作るヘルパー。
    fn amount_only_schema() -> Schema {
        Schema::new(vec![Column::new("amount", DataType::BigInt, true)])
    }

    fn amount_only_row(amount: Option<i64>) -> Tuple {
        let schema = amount_only_schema();
        Tuple::new(&schema, vec![amount.map(Value::BigInt).unwrap_or(Value::Null)]).unwrap()
    }

    fn exec_over_amount_rows(rows: Vec<Tuple>) -> Box<dyn Executor> {
        Box::new(ValuesRowsExec { schema: amount_only_schema(), rows: rows.into_iter() })
    }

    #[test]
    fn sort_orders_ascending_with_nulls_first() {
        let select = bind_select_orders("SELECT amount FROM orders ORDER BY amount");
        let logical = crate::logical_plan::build_select(select);
        let physical = optimize(logical, None, &NoStats);
        let PhysicalPlan::Sort(sort) = physical else { panic!("Sortを期待した") };
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![amount_only_row(Some(3)), amount_only_row(None), amount_only_row(Some(1))];
        let mut exec = SortExec::new(exec_over_amount_rows(rows), &sort.keys, &functions).unwrap();
        let result = collect_all(&mut exec);
        let values: Vec<&Value> = result.iter().map(|t| &t.values()[0]).collect();
        assert_eq!(values, vec![&Value::Null, &Value::BigInt(1), &Value::BigInt(3)]);
    }

    #[test]
    fn sort_desc_puts_nulls_last() {
        let select = bind_select_orders("SELECT amount FROM orders ORDER BY amount DESC");
        let logical = crate::logical_plan::build_select(select);
        let physical = optimize(logical, None, &NoStats);
        let PhysicalPlan::Sort(sort) = physical else { panic!("Sortを期待した") };
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![amount_only_row(Some(3)), amount_only_row(None), amount_only_row(Some(1))];
        let mut exec = SortExec::new(exec_over_amount_rows(rows), &sort.keys, &functions).unwrap();
        let result = collect_all(&mut exec);
        let values: Vec<&Value> = result.iter().map(|t| &t.values()[0]).collect();
        assert_eq!(values, vec![&Value::BigInt(3), &Value::BigInt(1), &Value::Null]);
    }

    #[test]
    fn sort_is_stable_among_equal_keys() {
        // `dept`が同じ行同士は、入力に現れた順序のまま残る(安定ソート)。
        let select = bind_select_orders("SELECT dept, amount FROM orders ORDER BY dept");
        let logical = crate::logical_plan::build_select(select);
        let physical = optimize(logical, None, &NoStats);
        let PhysicalPlan::Sort(sort) = physical else { panic!("Sortを期待した") };
        let functions = FunctionRegistry::with_builtins();
        let rows = vec![
            order_row(Some("eng"), Some(1)),
            order_row(Some("eng"), Some(2)),
            order_row(Some("eng"), Some(3)),
        ];
        let mut exec = SortExec::new(exec_over_rows(rows), &sort.keys, &functions).unwrap();
        let result = collect_all(&mut exec);
        let amounts: Vec<&Value> = result.iter().map(|t| &t.values()[1]).collect();
        assert_eq!(amounts, vec![&Value::BigInt(1), &Value::BigInt(2), &Value::BigInt(3)]);
    }

    // ---- DistinctExec ----

    #[test]
    fn distinct_treats_null_rows_as_duplicates_of_each_other() {
        let rows = vec![order_row(None, None), order_row(None, None), order_row(Some("eng"), Some(1))];
        let mut exec = DistinctExec::new(exec_over_rows(rows));
        let result = collect_all(&mut exec);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn distinct_returns_the_first_row_without_buffering_the_whole_input() {
        let rows: Vec<Tuple> = (0..1000).map(|i| order_row(Some("eng"), Some(i))).collect();
        let pulled = Rc::new(Cell::new(0));
        let leaf = CountingExecutor { schema: orders_schema(), rows: rows.into_iter(), pulled: pulled.clone() };
        let mut exec = DistinctExec::new(Box::new(leaf));
        assert!(exec.next().unwrap().is_some());
        // 1,000行のうち1行目ですでに新規行なので、1回しか子を引かない。
        assert_eq!(pulled.get(), 1);
    }

    // ---- LimitExec ----

    #[test]
    fn limit_stops_pulling_the_child_once_satisfied() {
        let rows: Vec<Tuple> = (0..1000).map(|i| order_row(Some("eng"), Some(i))).collect();
        let pulled = Rc::new(Cell::new(0));
        let leaf = CountingExecutor { schema: orders_schema(), rows: rows.into_iter(), pulled: pulled.clone() };
        let mut exec = LimitExec::new(Box::new(leaf), Some(3), None);

        assert!(exec.next().unwrap().is_some());
        assert!(exec.next().unwrap().is_some());
        assert!(exec.next().unwrap().is_some());
        assert!(exec.next().unwrap().is_none());
        // 3行返した後、4回目の`next()`は子を1回も引かずに`None`を返す。
        assert_eq!(pulled.get(), 3);
    }

    #[test]
    fn limit_with_offset_skips_then_takes() {
        let rows: Vec<Tuple> = (0..5).map(|i| order_row(Some("eng"), Some(i))).collect();
        let mut exec = LimitExec::new(exec_over_rows(rows), Some(2), Some(2));
        let result = collect_all(&mut exec);
        let amounts: Vec<&Value> = result.iter().map(|t| &t.values()[1]).collect();
        assert_eq!(amounts, vec![&Value::BigInt(2), &Value::BigInt(3)]);
    }

    #[test]
    fn limit_zero_returns_no_rows() {
        let rows: Vec<Tuple> = (0..5).map(|i| order_row(Some("eng"), Some(i))).collect();
        let mut exec = LimitExec::new(exec_over_rows(rows), Some(0), None);
        assert!(collect_all(&mut exec).is_empty());
    }

    // ---- Join(第22章) ----

    /// `a(id, x)`・`b(id, y)`という2テーブルのカタログ。`b.id`はNULLキーの
    /// テストのためnullableにしてある。
    fn ab_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table("a", Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("x", DataType::Text, true)]))
            .unwrap();
        catalog
            .create_table("b", Schema::new(vec![Column::new("id", DataType::BigInt, true), Column::new("y", DataType::Text, true)]))
            .unwrap();
        catalog
    }

    fn bind_select_ab(sql: &str) -> crate::binder::BoundSelect {
        let catalog = ab_catalog();
        let functions = FunctionRegistry::with_builtins();
        let statement = parse_statement(sql).unwrap();
        match Binder::new(&catalog, &functions, sql).bind(statement).unwrap() {
            BoundStatement::Select(select) => *select,
            other => panic!("Selectを期待したが{other:?}が返った"),
        }
    }

    fn a_schema() -> Schema {
        Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("x", DataType::Text, true)])
    }

    fn b_schema() -> Schema {
        Schema::new(vec![Column::new("id", DataType::BigInt, true), Column::new("y", DataType::Text, true)])
    }

    fn a_row(id: i64, x: &str) -> Tuple {
        Tuple::new(&a_schema(), vec![Value::BigInt(id), Value::Text(x.to_string())]).unwrap()
    }

    fn b_row(id: Option<i64>, y: &str) -> Tuple {
        let id = id.map(Value::BigInt).unwrap_or(Value::Null);
        Tuple::new(&b_schema(), vec![id, Value::Text(y.to_string())]).unwrap()
    }

    fn exec_over_a_rows(rows: Vec<Tuple>) -> Box<dyn Executor> {
        Box::new(ValuesRowsExec { schema: a_schema(), rows: rows.into_iter() })
    }

    fn exec_over_b_rows(rows: Vec<Tuple>) -> Box<dyn Executor> {
        Box::new(ValuesRowsExec { schema: b_schema(), rows: rows.into_iter() })
    }

    /// `condition`(結合後スキーマ上のフラットな添字を持つ)から、`optimize`が
    /// 行うのと同じ手順でHash Joinの鍵を取り出す。取り出せなければ`None`。
    fn equi_keys(condition: &BoundExpr) -> Option<Vec<(BoundExpr, BoundExpr)>> {
        let left_len = a_schema().len();
        split_equi_join_keys(condition, left_len)
            .map(|keys| keys.into_iter().map(|(l, r)| (l, shift_column_index(&r, left_len))).collect())
    }

    #[test]
    fn optimize_chooses_hash_join_for_an_equality_condition() {
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id");
        let physical = optimize(build_select(select), None, &NoStats);
        assert!(physical.to_string().contains("HashJoin"));
    }

    #[test]
    fn optimize_chooses_nested_loop_join_for_a_non_equality_condition() {
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id > b.id");
        let physical = optimize(build_select(select), None, &NoStats);
        assert!(physical.to_string().contains("NestedLoopJoin"));
    }

    #[test]
    fn nested_loop_and_hash_join_produce_identical_results_for_an_equi_join() {
        // NLJとHash Joinは異なるアルゴリズムだが、同じ入力・同じ等値条件に
        // 対しては同じ行集合を、同じ順序(`left`優先、`right`は元のスキャン順)
        // で返すはずである。この一致を、同一クエリを両方の演算子で実行して
        // 突き合わせることで確認する。
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id");
        let condition = select.joins[0].condition.clone();
        let keys = equi_keys(&condition).expect("等値条件のはず");

        // `a.id = 2`が2行、`b.id = 2`が2行あるので、この組み合わせだけで
        // 2×2 = 4行のマッチが生まれる(重複キーの多重集合としての結合)。
        let a_rows = vec![a_row(1, "a1"), a_row(2, "a2"), a_row(2, "a2b"), a_row(3, "a3")];
        let b_rows = vec![b_row(Some(2), "b2"), b_row(None, "bnull"), b_row(Some(2), "b2b"), b_row(Some(9), "bnomatch")];

        let functions = FunctionRegistry::with_builtins();
        let mut nlj =
            NestedLoopJoinExec::new(exec_over_a_rows(a_rows.clone()), exec_over_b_rows(b_rows.clone()), &condition, &functions)
                .unwrap();
        let mut hash = HashJoinExec::new(exec_over_a_rows(a_rows), exec_over_b_rows(b_rows), &keys, &functions).unwrap();

        let nlj_rows = collect_all(&mut nlj);
        let hash_rows = collect_all(&mut hash);
        assert_eq!(nlj_rows.len(), 4);
        assert_eq!(nlj_rows, hash_rows);
    }

    #[test]
    fn hash_join_excludes_null_keys_from_both_sides() {
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id");
        let condition = select.joins[0].condition.clone();
        let keys = equi_keys(&condition).expect("等値条件のはず");

        let a_rows = vec![a_row(1, "a1")];
        let b_rows = vec![b_row(Some(1), "b1"), b_row(None, "bnull")];

        let functions = FunctionRegistry::with_builtins();
        let mut hash = HashJoinExec::new(exec_over_a_rows(a_rows), exec_over_b_rows(b_rows), &keys, &functions).unwrap();
        let rows = collect_all(&mut hash);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn nested_loop_join_excludes_null_keys_via_three_valued_logic() {
        // NLJは専用のNULL処理を持たず、`WHERE`と同じ三値論理(`eval_bound_expr`・
        // `predicate_matches`)を経由するだけでNULLキーの行が自然に除外される
        // ことを確認する。
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id");
        let condition = select.joins[0].condition.clone();

        let a_rows = vec![a_row(1, "a1")];
        let b_rows = vec![b_row(Some(1), "b1"), b_row(None, "bnull")];

        let functions = FunctionRegistry::with_builtins();
        let mut nlj = NestedLoopJoinExec::new(exec_over_a_rows(a_rows), exec_over_b_rows(b_rows), &condition, &functions).unwrap();
        let rows = collect_all(&mut nlj);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn join_against_an_empty_right_table_returns_no_rows() {
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id");
        let condition = select.joins[0].condition.clone();
        let keys = equi_keys(&condition).expect("等値条件のはず");

        let a_rows = vec![a_row(1, "a1"), a_row(2, "a2")];

        let functions = FunctionRegistry::with_builtins();
        let mut nlj =
            NestedLoopJoinExec::new(exec_over_a_rows(a_rows.clone()), exec_over_b_rows(Vec::new()), &condition, &functions)
                .unwrap();
        let mut hash = HashJoinExec::new(exec_over_a_rows(a_rows), exec_over_b_rows(Vec::new()), &keys, &functions).unwrap();
        assert!(collect_all(&mut nlj).is_empty());
        assert!(collect_all(&mut hash).is_empty());
    }

    #[test]
    fn join_against_an_empty_left_table_returns_no_rows() {
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id");
        let condition = select.joins[0].condition.clone();
        let keys = equi_keys(&condition).expect("等値条件のはず");

        let b_rows = vec![b_row(Some(1), "b1")];

        let functions = FunctionRegistry::with_builtins();
        let mut nlj =
            NestedLoopJoinExec::new(exec_over_a_rows(Vec::new()), exec_over_b_rows(b_rows.clone()), &condition, &functions)
                .unwrap();
        let mut hash = HashJoinExec::new(exec_over_a_rows(Vec::new()), exec_over_b_rows(b_rows), &keys, &functions).unwrap();
        assert!(collect_all(&mut nlj).is_empty());
        assert!(collect_all(&mut hash).is_empty());
    }

    #[test]
    fn split_equi_join_keys_handles_a_conjunction_of_two_equalities() {
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id AND a.x = b.y");
        let condition = select.joins[0].condition.clone();
        let keys = split_equi_join_keys(&condition, a_schema().len()).expect("2つの等値条件のはず");
        assert_eq!(keys.len(), 2);
    }

    #[test]
    fn split_equi_join_keys_rejects_an_or_condition() {
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id = b.id OR a.x = b.y");
        let condition = select.joins[0].condition.clone();
        assert!(split_equi_join_keys(&condition, a_schema().len()).is_none());
    }

    #[test]
    fn split_equi_join_keys_rejects_a_condition_mixing_both_sides_on_one_term() {
        // `a.id + b.id = 3`は、左辺が左右両方のテーブルを参照しており、
        // どちらか一方の側だけを参照する式という前提を満たさない。
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id + b.id = 3");
        let condition = select.joins[0].condition.clone();
        assert!(split_equi_join_keys(&condition, a_schema().len()).is_none());
    }

    #[test]
    fn multi_way_join_chains_three_tables_left_deep() {
        let mut catalog = ab_catalog();
        catalog
            .create_table("c", Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("z", DataType::Text, true)]))
            .unwrap();
        let functions = FunctionRegistry::with_builtins();
        let sql = "SELECT a.id, c.z FROM a JOIN b ON a.id = b.id JOIN c ON a.id = c.id";
        let statement = parse_statement(sql).unwrap();
        let select = match Binder::new(&catalog, &functions, sql).bind(statement).unwrap() {
            BoundStatement::Select(select) => *select,
            other => panic!("Selectを期待したが{other:?}が返った"),
        };
        let physical = optimize(build_select(select), None, &NoStats);
        // 左深い木: 一番外側(根に近い)のJoinの子にもう1つJoinが現れる。
        let text = physical.to_string();
        assert_eq!(text.matches("Join").count(), 2);
    }

    // ---- Physical Properties: output_ordering・sort_is_already_satisfied(第29章) ----
    //
    // 第28章の限界(`ch28-cost-model.md`の「この章の限界」)がまだ残っている
    // ため、`cheapest`が実際にIndex Range Scanを選ぶ場面(Histogramの粒度に
    // 対して、この章のコスト定数ではIndex Range ScanがSeqScanにほぼ勝てない)
    // をANALYZE統計だけで自然に再現するのは難しい。ここでは`output_ordering`と
    // `sort_is_already_satisfied`(`optimize`の`LogicalPlan::Sort`の分岐が
    // 実際に呼ぶのと同じ関数)を、`PhysicalPlan`を直接組み立てて検証する。

    fn orders_index_scan_schema() -> Schema {
        Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("amount", DataType::BigInt, true)])
    }

    fn range_index_scan_on_amount() -> PhysicalPlan {
        PhysicalPlan::IndexScan(IndexScanNode {
            table_id: TableId(0),
            table_name: "orders".to_string(),
            schema: orders_index_scan_schema(),
            index_name: "idx_amount".to_string(),
            column_name: "amount".to_string(),
            kind: IndexScanKind::Range { lower: Bound::Included(Value::BigInt(100)), upper: Bound::Unbounded },
        })
    }

    fn point_index_scan_on_amount() -> PhysicalPlan {
        PhysicalPlan::IndexScan(IndexScanNode {
            table_id: TableId(0),
            table_name: "orders".to_string(),
            schema: orders_index_scan_schema(),
            index_name: "idx_amount".to_string(),
            column_name: "amount".to_string(),
            kind: IndexScanKind::Point(Value::BigInt(100)),
        })
    }

    fn asc_sort_key(column_index: usize) -> logical_plan::SortKey {
        logical_plan::SortKey {
            expr: BoundExpr::ColumnRef {
                table_ordinal: 0,
                column_index,
                name: "amount".to_string(),
                data_type: DataType::BigInt,
                span: crate::lexer::Span::new(0, 0),
            },
            desc: false,
        }
    }

    #[test]
    fn output_ordering_range_index_scan_returns_the_scanned_column() {
        assert_eq!(output_ordering(&range_index_scan_on_amount()), Some(1));
    }

    #[test]
    fn output_ordering_point_index_scan_returns_none() {
        // 一致行が常に同じキー値を持つため、Sortを省く役に立たない
        // (`output_ordering`のドキュメント参照)。
        assert_eq!(output_ordering(&point_index_scan_on_amount()), None);
    }

    #[test]
    fn output_ordering_seq_scan_returns_none() {
        let scan = PhysicalPlan::SeqScan(SeqScanNode { table_id: TableId(0), table_name: "orders".to_string(), schema: orders_index_scan_schema() });
        assert_eq!(output_ordering(&scan), None);
    }

    #[test]
    fn output_ordering_passes_through_filter_unchanged() {
        let filter = PhysicalPlan::Filter(FilterNode { input: Box::new(range_index_scan_on_amount()), predicate: bound_true_predicate() });
        assert_eq!(output_ordering(&filter), Some(1));
    }

    #[test]
    fn output_ordering_survives_a_projection_that_keeps_the_ordered_column() {
        // `amount`(添字1)だけを残す`Projection`。出力側では添字0になる。
        let projection = PhysicalPlan::Projection(ProjectionNode {
            input: Box::new(range_index_scan_on_amount()),
            projection: vec![BoundSelectItem { expr: asc_sort_key(1).expr, output_name: "amount".to_string() }],
        });
        assert_eq!(output_ordering(&projection), Some(0));
    }

    #[test]
    fn output_ordering_is_lost_when_a_projection_drops_the_ordered_column() {
        let projection = PhysicalPlan::Projection(ProjectionNode {
            input: Box::new(range_index_scan_on_amount()),
            projection: vec![BoundSelectItem {
                expr: BoundExpr::ColumnRef { table_ordinal: 0, column_index: 0, name: "id".to_string(), data_type: DataType::BigInt, span: crate::lexer::Span::new(0, 0) },
                output_name: "id".to_string(),
            }],
        });
        assert_eq!(output_ordering(&projection), None);
    }

    #[test]
    fn output_ordering_survives_the_left_side_of_every_join_kind() {
        // `HashJoin`・`NestedLoopJoin`・`IndexNestedLoopJoin`は、どれも
        // `left`の行を1件ずつ`next()`で引いた順序を外側ループに使うため、
        // `left`の出力順序をそのまま引き継ぐ(`output_ordering`のドキュメント
        // 参照)。`right`(内側)は素通しの対象外であることも合わせて確認する。
        let left = range_index_scan_on_amount(); // 出力順序 = Some(1)
        let right_schema = Schema::new(vec![Column::new("id", DataType::BigInt, false)]);
        let right_scan = || PhysicalPlan::SeqScan(SeqScanNode { table_id: TableId(1), table_name: "b".to_string(), schema: right_schema.clone() });

        let hash = PhysicalPlan::HashJoin(HashJoinNode { left: Box::new(left.clone()), right: Box::new(right_scan()), kind: JoinKind::Inner, keys: Vec::new(), condition: bound_true_predicate() });
        assert_eq!(output_ordering(&hash), Some(1));

        let nlj = PhysicalPlan::NestedLoopJoin(NestedLoopJoinNode { left: Box::new(left.clone()), right: Box::new(right_scan()), kind: JoinKind::Inner, condition: bound_true_predicate() });
        assert_eq!(output_ordering(&nlj), Some(1));

        let inlj = PhysicalPlan::IndexNestedLoopJoin(IndexNestedLoopJoinNode {
            left: Box::new(left),
            kind: JoinKind::Inner,
            condition: bound_true_predicate(),
            outer_key: asc_sort_key(1).expr,
            table_id: TableId(1),
            table_name: "b".to_string(),
            schema: right_schema.clone(),
            index_name: "idx_b".to_string(),
            column_name: "id".to_string(),
        });
        assert_eq!(output_ordering(&inlj), Some(1));

        // `right`側に順序があっても(内側は素通しの対象外なので)引き継がれない。
        let right_ordered = range_index_scan_on_amount();
        let hash_from_right =
            PhysicalPlan::HashJoin(HashJoinNode { left: Box::new(right_scan()), right: Box::new(right_ordered), kind: JoinKind::Inner, keys: Vec::new(), condition: bound_true_predicate() });
        assert_eq!(output_ordering(&hash_from_right), None);
    }

    #[test]
    fn output_ordering_hash_join_build_side_does_not_count_as_ordered() {
        // `HashJoin`自身(`left`が順序を持たない場合)はNone。ハッシュテーブル
        // 経由の内側走査順は未規定であるという前提を裏から確認する。
        let left = PhysicalPlan::SeqScan(SeqScanNode { table_id: TableId(0), table_name: "a".to_string(), schema: orders_index_scan_schema() });
        let right = range_index_scan_on_amount();
        let hash = PhysicalPlan::HashJoin(HashJoinNode { left: Box::new(left), right: Box::new(right), kind: JoinKind::Inner, keys: Vec::new(), condition: bound_true_predicate() });
        assert_eq!(output_ordering(&hash), None);
    }

    #[test]
    fn sort_is_already_satisfied_for_a_single_ascending_key_matching_a_range_scan() {
        assert!(sort_is_already_satisfied(&[asc_sort_key(1)], &range_index_scan_on_amount()));
    }

    #[test]
    fn sort_is_already_satisfied_rejects_a_descending_key() {
        let mut key = asc_sort_key(1);
        key.desc = true;
        assert!(!sort_is_already_satisfied(&[key], &range_index_scan_on_amount()));
    }

    #[test]
    fn sort_is_already_satisfied_rejects_multiple_keys() {
        assert!(!sort_is_already_satisfied(&[asc_sort_key(1), asc_sort_key(0)], &range_index_scan_on_amount()));
    }

    #[test]
    fn sort_is_already_satisfied_rejects_a_different_column() {
        assert!(!sort_is_already_satisfied(&[asc_sort_key(0)], &range_index_scan_on_amount()));
    }

    #[test]
    fn sort_is_already_satisfied_rejects_a_point_scan() {
        assert!(!sort_is_already_satisfied(&[asc_sort_key(1)], &point_index_scan_on_amount()));
    }
}
