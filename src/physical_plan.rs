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

use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;

use crate::ast::{AggregateFunc, BinaryOperator, Expr, JoinKind};
use crate::binder::{AggregateCall, BoundAssignment, BoundExpr, BoundSelectItem};
use crate::error::{DbError, DbResult};
use crate::eval::{FunctionRegistry, eval_bound_expr, eval_expr};
use crate::executor::predicate_matches;
use crate::heap_file::Scan as HeapScan;
use crate::ids::TableId;
use crate::logical_plan::{self, LogicalPlan, SortKey};
use crate::storage::Storage;
use crate::storage_mem::MemTable;
use crate::tuple_codec::decode_tuple;
use crate::types::{Row, Schema, Tuple, Value, compare_values};

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
    Values(ValuesNode),
    Filter(FilterNode),
    /// `ON`が等値条件の連言(AND)へ分解できるときに選ばれる(第22章、
    /// `split_equi_join_keys`)。
    HashJoin(HashJoinNode),
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
/// 索引が無いこの章では`Scan`は必ず`SeqScan`になり、他のノードも形を
/// そのまま引き継ぐだけである。それでも変換の名前を`optimize`にしたのは、
/// 第25章でIndex Scanが加わった後もこの関数が「実行アルゴリズムを選ぶ」
/// 責務の置き場所であり続けるからである(この章の実装はその選択がまだ
/// 1択しかない特殊ケースにすぎない)。
pub fn optimize(plan: LogicalPlan) -> PhysicalPlan {
    match plan {
        LogicalPlan::Scan(scan) => PhysicalPlan::SeqScan(SeqScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name,
            schema: scan.schema,
        }),
        LogicalPlan::Values(values) => PhysicalPlan::Values(ValuesNode { schema: values.schema, rows: values.rows }),
        LogicalPlan::Filter(filter) => {
            PhysicalPlan::Filter(FilterNode { input: Box::new(optimize(*filter.input)), predicate: filter.predicate })
        }
        LogicalPlan::Join(join) => {
            let left = optimize(*join.left);
            let right = optimize(*join.right);
            let left_len = left.output_schema().len();
            match split_equi_join_keys(&join.condition, left_len) {
                Some(keys) => {
                    let keys = keys
                        .into_iter()
                        .map(|(left_key, right_key)| (left_key, shift_column_index(&right_key, left_len)))
                        .collect();
                    PhysicalPlan::HashJoin(HashJoinNode {
                        left: Box::new(left),
                        right: Box::new(right),
                        kind: join.kind,
                        keys,
                        condition: join.condition,
                    })
                }
                None => PhysicalPlan::NestedLoopJoin(NestedLoopJoinNode {
                    left: Box::new(left),
                    right: Box::new(right),
                    kind: join.kind,
                    condition: join.condition,
                }),
            }
        }
        LogicalPlan::Aggregate(aggregate) => PhysicalPlan::Aggregate(AggregateNode {
            input: Box::new(optimize(*aggregate.input)),
            group_by: aggregate.group_by,
            calls: aggregate.calls,
            schema: aggregate.schema,
        }),
        LogicalPlan::Projection(projection) => PhysicalPlan::Projection(ProjectionNode {
            input: Box::new(optimize(*projection.input)),
            projection: projection.projection,
        }),
        LogicalPlan::Distinct(distinct) => {
            PhysicalPlan::Distinct(DistinctNode { input: Box::new(optimize(*distinct.input)) })
        }
        LogicalPlan::Sort(sort) => {
            PhysicalPlan::Sort(SortNode { input: Box::new(optimize(*sort.input)), keys: sort.keys })
        }
        LogicalPlan::Limit(limit) => PhysicalPlan::Limit(LimitNode {
            input: Box::new(optimize(*limit.input)),
            limit: limit.limit,
            offset: limit.offset,
        }),
        LogicalPlan::Insert(insert) => PhysicalPlan::Insert(InsertNode {
            table_id: insert.table_id,
            table_name: insert.table_name,
            schema: insert.schema,
            columns: insert.columns,
            input: Box::new(optimize(*insert.input)),
        }),
        LogicalPlan::Update(update) => PhysicalPlan::Update(UpdateNode {
            table_id: update.table_id,
            table_name: update.table_name,
            schema: update.schema,
            assignments: update.assignments,
            predicate: update.predicate,
            input: Box::new(optimize(*update.input)),
        }),
        LogicalPlan::Delete(delete) => PhysicalPlan::Delete(DeleteNode {
            table_id: delete.table_id,
            table_name: delete.table_name,
            schema: delete.schema,
            predicate: delete.predicate,
            input: Box::new(optimize(*delete.input)),
        }),
    }
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
/// この章の物理選択はこの1点だけを見る単純なルールである。統計情報や
/// コストを比較して選ぶわけではない(第28章)。等値条件が1つも取り出せない
/// `ON true`のような結合(実質的な直積)も、この関数が`None`を返すことで
/// 自然に`NestedLoopJoin`へ倒れる。
fn split_equi_join_keys(condition: &BoundExpr, left_len: usize) -> Option<Vec<(BoundExpr, BoundExpr)>> {
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
fn collect_conjuncts<'a>(expr: &'a BoundExpr, out: &mut Vec<&'a BoundExpr>) {
    match strip_paren(expr) {
        BoundExpr::BinaryOp { op: BinaryOperator::And, lhs, rhs, .. } => {
            collect_conjuncts(lhs, out);
            collect_conjuncts(rhs, out);
        }
        other => out.push(other),
    }
}

fn strip_paren(expr: &BoundExpr) -> &BoundExpr {
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
fn columns_side(expr: &BoundExpr, left_len: usize) -> Option<Side> {
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
enum Side {
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
fn shift_column_index(expr: &BoundExpr, delta: usize) -> BoundExpr {
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
            PhysicalPlan::Values(values) => values.schema.clone(),
            PhysicalPlan::Filter(filter) => filter.input.output_schema(),
            PhysicalPlan::NestedLoopJoin(join) => {
                logical_plan::join_schema(&join.left.output_schema(), &join.right.output_schema())
            }
            PhysicalPlan::HashJoin(join) => {
                logical_plan::join_schema(&join.left.output_schema(), &join.right.output_schema())
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

    fn children(&self) -> Vec<&PhysicalPlan> {
        match self {
            PhysicalPlan::SeqScan(_) | PhysicalPlan::Values(_) => Vec::new(),
            PhysicalPlan::Filter(filter) => vec![&filter.input],
            PhysicalPlan::NestedLoopJoin(join) => vec![&join.left, &join.right],
            PhysicalPlan::HashJoin(join) => vec![&join.left, &join.right],
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
        Ok(())
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
        let physical = optimize(build_select(select));
        assert_eq!(physical.to_string(), "Projection(name)\n  └─ Filter(id = 42)\n    └─ SeqScan(users)\n");
    }

    #[test]
    fn optimize_preserves_output_schema() {
        let select = bind_select("SELECT id, name FROM users");
        let physical = optimize(build_select(select));
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
        let physical = optimize(logical);
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
        let physical = optimize(logical);
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
        let physical = optimize(logical);
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
        let physical = optimize(build_select(select));
        assert!(physical.to_string().contains("HashJoin"));
    }

    #[test]
    fn optimize_chooses_nested_loop_join_for_a_non_equality_condition() {
        let select = bind_select_ab("SELECT a.id FROM a JOIN b ON a.id > b.id");
        let physical = optimize(build_select(select));
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
        let physical = optimize(build_select(select));
        // 左深い木: 一番外側(根に近い)のJoinの子にもう1つJoinが現れる。
        let text = physical.to_string();
        assert_eq!(text.matches("Join").count(), 2);
    }
}
