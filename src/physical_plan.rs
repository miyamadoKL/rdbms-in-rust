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

use std::fmt;

use crate::ast::Expr;
use crate::binder::{BoundAssignment, BoundExpr, BoundSelectItem};
use crate::error::DbResult;
use crate::eval::{FunctionRegistry, eval_bound_expr, eval_expr};
use crate::executor::predicate_matches;
use crate::heap_file::Scan as HeapScan;
use crate::ids::TableId;
use crate::logical_plan::{self, LogicalPlan};
use crate::storage::Storage;
use crate::storage_mem::MemTable;
use crate::tuple_codec::decode_tuple;
use crate::types::{Row, Schema, Tuple};

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
    Projection(ProjectionNode),
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

/// [`PhysicalPlan::Projection`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionNode {
    pub input: Box<PhysicalPlan>,
    pub projection: Vec<BoundSelectItem>,
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
        LogicalPlan::Projection(projection) => PhysicalPlan::Projection(ProjectionNode {
            input: Box::new(optimize(*projection.input)),
            projection: projection.projection,
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

impl PhysicalPlan {
    /// この演算子が返す行の列構成。ルールは`LogicalPlan::output_schema`と
    /// 同じで、`Projection`の列構成は同じ[`logical_plan::projection_schema`]
    /// を呼んで決める(決め方を2箇所に分けないという第18章の方針を踏襲する)。
    pub fn output_schema(&self) -> Schema {
        match self {
            PhysicalPlan::SeqScan(scan) => scan.schema.clone(),
            PhysicalPlan::Values(values) => values.schema.clone(),
            PhysicalPlan::Filter(filter) => filter.input.output_schema(),
            PhysicalPlan::Projection(projection) => {
                logical_plan::projection_schema(&projection.input.output_schema(), &projection.projection)
            }
            PhysicalPlan::Insert(_) | PhysicalPlan::Update(_) | PhysicalPlan::Delete(_) => Schema::new(Vec::new()),
        }
    }

    fn children(&self) -> Vec<&PhysicalPlan> {
        match self {
            PhysicalPlan::SeqScan(_) | PhysicalPlan::Values(_) => Vec::new(),
            PhysicalPlan::Filter(filter) => vec![&filter.input],
            PhysicalPlan::Projection(projection) => vec![&projection.input],
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
            PhysicalPlan::Projection(projection) => {
                let items: Vec<&str> = projection.projection.iter().map(|item| item.output_name.as_str()).collect();
                format!("Projection({})", items.join(", "))
            }
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
            BoundStatement::Select(select) => select,
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
}
