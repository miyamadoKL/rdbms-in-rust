//! Bound ASTを、関係代数の演算子から成る**Logical Plan**へ変換する。
//!
//! 第17章の`Binder`が作る`BoundStatement`は、名前と型をすでに解決している。
//! それでも`BoundSelect`自体は`tables`・`projection`・`predicate`という、
//! `SELECT`という構文の形をそのまま引き継いだフィールドの集まりでしかない。
//! `WHERE`を先に評価するのか`SELECT`の対象式を先に評価するのかは、この形からは
//! 読み取れず、`Database::execute_select_with_from`(第17章まで)が
//! Sequential Scan→Filter→Projectionという順序をコードの並びとして手続き的に
//! 書き下ろすことで初めて確定していた。
//!
//! この章の[`LogicalPlan`]は、その順序を木の親子関係として表現し直す。
//! `SELECT name FROM users WHERE id = 42`は次の木になる。
//!
//! ```text
//! Projection(name)
//!   └─ Filter(id = 42)
//!     └─ Scan(users)
//! ```
//!
//! 演算子は次の11種類にとどめる。`Scan`・`Values`・`Filter`・`Projection`・
//! `Aggregate`・`Distinct`・`Sort`・`Limit`は`SELECT`が、`Insert`・`Update`・
//! `Delete`はそれぞれの文が使う。`Join`にあたる構文はまだこのクレートに無い
//! ため、この章では対応する演算子を作らない(第22章で追加する余地として残す)。
//!
//! `LogicalPlan`が確定させるのは「何を計算するか」という演算子の並びと
//! 依存関係だけであり、「どう計算するか」(`Scan`が全件走査になるのか索引を
//! 使うのか)は決めない。索引はまだこのクレートに無いので、この区別は
//! 第23〜25章でB+TreeとIndex Scanが揃うまでは意味を持たないが、`Scan`という
//! 名前は最初から「走査する対象」だけを表し、「どう走査するか」を含まない
//! 名前として選んである。
//!
//! # `SELECT`の評価順序と演算子の合成(第21章)
//!
//! `GROUP BY`・`HAVING`・集約関数・`DISTINCT`・`ORDER BY`・`LIMIT`/`OFFSET`が
//! 揃ったことで、[`build_select`]が組み立てる木の形は標準SQLが定める
//! `SELECT`の論理的な評価順序をそのまま反映するようになった。
//!
//! ```text
//! FROM → WHERE → GROUP BY → HAVING → SELECT(射影) → DISTINCT → ORDER BY → LIMIT/OFFSET
//! ```
//!
//! `LogicalPlan`の木では、この順序が根から葉への深さとして現れる(根に近いほど
//! 後段)。
//!
//! ```text
//! Limit
//!   └─ Sort
//!     └─ Distinct
//!       └─ Projection
//!         └─ Filter(HAVING)
//!           └─ Aggregate
//!             └─ Filter(WHERE)
//!               └─ Scan
//! ```
//!
//! `HAVING`を独立した演算子にせず`Filter`を再利用しているのは、`HAVING`が
//! 「行を絞り込む」という点で`WHERE`と全く同じ演算だからである。違うのは
//! 述語が評価する行の由来(`WHERE`は`Scan`が返す生の行、`HAVING`は`Aggregate`が
//! 返すグループごとの集約結果)だけであり、これは`FilterNode::input`が指す
//! 子が変わることで表現できる。

use std::fmt;

use crate::ast::{BinaryOperator, Expr, JoinKind, UnaryOperator};
use crate::binder::{
    AggregateCall, BoundAssignment, BoundDelete, BoundExpr, BoundInsert, BoundSelect, BoundSelectItem, BoundUpdate,
};
use crate::ids::TableId;
use crate::types::{Column, DataType, Schema};

/// 関係代数の演算子1個。
///
/// `Filter`・`Projection`・`Aggregate`・`Distinct`・`Sort`・`Limit`・
/// `Insert`・`Update`・`Delete`は、それぞれ1個の子(`input`)を持つ。`Scan`・
/// `Values`は子を持たない葉である。
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// テーブル全体を走査する。
    Scan(ScanNode),
    /// `VALUES`が並べる、リテラル式の行の並び。`FROM`を伴わない`SELECT`は、
    /// 列を1つも持たない行を1件だけ持つ`Values`を入力とみなす。
    Values(ValuesNode),
    /// `predicate`が`TRUE`になった行だけを残す。`WHERE`と`HAVING`のどちらも
    /// このノードで表す(モジュール冒頭の説明を参照)。
    Filter(FilterNode),
    /// `left`・`right`を`condition`に従って結合する(第22章)。`left`・`right`
    /// の出力を1行ずつ連結した行(結合後スキーマ)を生成する。複数の`JOIN`は
    /// 左深い木(left-deep tree)として表現し、`n`個の`JOIN`を持つ`FROM`は
    /// `n`個の`Join`ノードが縦に連なる形になる(`build_select`参照)。
    Join(JoinNode),
    /// `group_by`の値が等しい行をグループ化し、グループごとに`calls`を計算する
    /// (第21章)。
    Aggregate(AggregateNode),
    /// 各行から`projection`が指す列・式だけを取り出す。
    Projection(ProjectionNode),
    /// 完全に一致する行を1つにまとめる(第21章)。
    Distinct(DistinctNode),
    /// `keys`に従って行を並べ替える(第21章)。
    Sort(SortNode),
    /// `offset`件飛ばしたうえで、先頭`limit`件だけを残す(第21章)。
    Limit(LimitNode),
    /// `input`(`Values`)の各行を`table_id`のテーブルへ書き込む。
    Insert(InsertNode),
    /// `input`が指すテーブルのうち、`predicate`に一致した行へ`assignments`を適用する。
    Update(UpdateNode),
    /// `input`が指すテーブルのうち、`predicate`に一致した行を取り除く。
    Delete(DeleteNode),
}

/// [`LogicalPlan::Scan`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct ScanNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
}

/// [`LogicalPlan::Values`]が持つ情報。
///
/// `rows`の各要素はまだ評価していない`Expr`のままである。`VALUES`は既存の行を
/// 参照する構文を持たない(第17章の`BoundInsert::rows`と同じ理由)ため、
/// `Binder`が解決すべき名前はそこに無く、`Expr`のまま持ち回ってよい。
///
/// `schema`は、この`Values`が生成する行の形を表す。`INSERT`の`VALUES`では
/// 挿入先のテーブルの`Schema`をそのまま使う(列名指定の有無に関わらず、行の
/// 置き場所はテーブルの列構成そのものだからである)。`FROM`を伴わない`SELECT`
/// では、列を1つも持たない空の`Schema`になる。
#[derive(Debug, Clone, PartialEq)]
pub struct ValuesNode {
    pub schema: Schema,
    pub rows: Vec<Vec<Expr>>,
}

/// [`LogicalPlan::Filter`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct FilterNode {
    pub input: Box<LogicalPlan>,
    pub predicate: BoundExpr,
}

/// [`LogicalPlan::Join`]が持つ情報(第22章)。
///
/// `condition`の中の`BoundExpr::ColumnRef`が持つ`column_index`は、
/// `left.output_schema()`と`right.output_schema()`を連結した結合後スキーマ
/// 上のフラットな添字である(`binder`モジュールの`BoundSelect`ドキュメント
/// 参照)。`left`が左深い木の途中(それ自体が別の`Join`)であっても、その
/// 出力列の並びは元の`FROM`に登場したテーブルの列をそのまま連結したものに
/// なるため、この規則は木の深さに関係なく成り立つ。
#[derive(Debug, Clone, PartialEq)]
pub struct JoinNode {
    pub left: Box<LogicalPlan>,
    pub right: Box<LogicalPlan>,
    pub kind: JoinKind,
    pub condition: BoundExpr,
}

/// [`LogicalPlan::Projection`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionNode {
    pub input: Box<LogicalPlan>,
    pub projection: Vec<BoundSelectItem>,
}

/// [`LogicalPlan::Aggregate`]が持つ情報(第21章)。
///
/// `group_by`は`input`の各行に対して評価するグループ化キーの式、`calls`は
/// グループごとに計算する集約関数呼び出しである。`schema`は`group_by`の列
/// (先頭)に`calls`の列(残り)を続けた出力列構成で、[`crate::binder::BoundAggregate::schema`]
/// と同じもの。
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateNode {
    pub input: Box<LogicalPlan>,
    pub group_by: Vec<BoundExpr>,
    pub calls: Vec<AggregateCall>,
    pub schema: Schema,
}

/// [`LogicalPlan::Distinct`]が持つ情報(第21章)。
#[derive(Debug, Clone, PartialEq)]
pub struct DistinctNode {
    pub input: Box<LogicalPlan>,
}

/// `ORDER BY`のキー1個(第21章)。`expr`は`input`が生成する行(`Sort`は
/// `Projection`の直後に置かれるため、常に射影後の出力行)に対して評価する。
#[derive(Debug, Clone, PartialEq)]
pub struct SortKey {
    pub expr: BoundExpr,
    pub desc: bool,
}

/// [`LogicalPlan::Sort`]が持つ情報(第21章)。
#[derive(Debug, Clone, PartialEq)]
pub struct SortNode {
    pub input: Box<LogicalPlan>,
    pub keys: Vec<SortKey>,
}

/// [`LogicalPlan::Limit`]が持つ情報(第21章)。`limit`・`offset`は
/// `Binder::eval_row_count_expr`が束縛の時点で評価し切った定数である。
#[derive(Debug, Clone, PartialEq)]
pub struct LimitNode {
    pub input: Box<LogicalPlan>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// [`LogicalPlan::Insert`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct InsertNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    /// 明示された列名を索引へ解決した並び。`None`なら`VALUES`の並びを
    /// そのままスキーマの列順とみなす(`BoundInsert::columns`と同じ意味)。
    pub columns: Option<Vec<usize>>,
    pub input: Box<LogicalPlan>,
}

/// [`LogicalPlan::Update`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub assignments: Vec<BoundAssignment>,
    pub predicate: Option<BoundExpr>,
    pub input: Box<LogicalPlan>,
}

/// [`LogicalPlan::Delete`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub predicate: Option<BoundExpr>,
    pub input: Box<LogicalPlan>,
}

impl LogicalPlan {
    /// この演算子が返す行の列構成。
    ///
    /// `Scan`・`Values`は自分が持つ`schema`をそのまま返す。`Filter`は行を
    /// 減らすだけで列構成を変えないため、子の`output_schema()`をそのまま返す。
    /// `Projection`は`projection`から新しい`Schema`を組み立てる
    /// ([`projection_schema`]。`executor::project`が実際の行を作るときの
    /// 列構成の決め方と同じ関数を使うので、2つがずれることはない)。
    /// `Insert`・`Update`・`Delete`は行を返さない文なので、列を1つも持たない
    /// 空の`Schema`を返す(`Database::execute`が組み立てる`QueryResult`が
    /// DDL・DML文に対して空の`Schema`を返すのと同じ約束事)。
    pub fn output_schema(&self) -> Schema {
        match self {
            LogicalPlan::Scan(scan) => scan.schema.clone(),
            LogicalPlan::Values(values) => values.schema.clone(),
            LogicalPlan::Filter(filter) => filter.input.output_schema(),
            LogicalPlan::Join(join) => join_schema(&join.left.output_schema(), &join.right.output_schema()),
            LogicalPlan::Aggregate(aggregate) => aggregate.schema.clone(),
            LogicalPlan::Projection(projection) => {
                projection_schema(&projection.input.output_schema(), &projection.projection)
            }
            LogicalPlan::Distinct(distinct) => distinct.input.output_schema(),
            LogicalPlan::Sort(sort) => sort.input.output_schema(),
            LogicalPlan::Limit(limit) => limit.input.output_schema(),
            LogicalPlan::Insert(_) | LogicalPlan::Update(_) | LogicalPlan::Delete(_) => Schema::new(Vec::new()),
        }
    }

    /// この演算子が直接持つ子。`Scan`・`Values`は葉なので空を返す。
    fn children(&self) -> Vec<&LogicalPlan> {
        match self {
            LogicalPlan::Scan(_) | LogicalPlan::Values(_) => Vec::new(),
            LogicalPlan::Filter(filter) => vec![&filter.input],
            LogicalPlan::Join(join) => vec![&join.left, &join.right],
            LogicalPlan::Aggregate(aggregate) => vec![&aggregate.input],
            LogicalPlan::Projection(projection) => vec![&projection.input],
            LogicalPlan::Distinct(distinct) => vec![&distinct.input],
            LogicalPlan::Sort(sort) => vec![&sort.input],
            LogicalPlan::Limit(limit) => vec![&limit.input],
            LogicalPlan::Insert(insert) => vec![&insert.input],
            LogicalPlan::Update(update) => vec![&update.input],
            LogicalPlan::Delete(delete) => vec![&delete.input],
        }
    }

    /// この演算子1個を表す、子を含まない1行のラベル。
    fn label(&self) -> String {
        match self {
            LogicalPlan::Scan(scan) => format!("Scan({})", scan.table_name),
            LogicalPlan::Values(values) => {
                let row_word = if values.rows.len() == 1 { "row" } else { "rows" };
                format!("Values({} {row_word})", values.rows.len())
            }
            LogicalPlan::Filter(filter) => format!("Filter({})", fmt_bound_expr(&filter.predicate)),
            LogicalPlan::Join(join) => format!("Join({}, {})", join.kind.name(), fmt_bound_expr(&join.condition)),
            LogicalPlan::Aggregate(aggregate) => {
                let group_by: Vec<String> = aggregate.group_by.iter().map(fmt_bound_expr).collect();
                let calls: Vec<String> = aggregate.calls.iter().map(fmt_aggregate_call).collect();
                format!("Aggregate(group_by=[{}], calls=[{}])", group_by.join(", "), calls.join(", "))
            }
            LogicalPlan::Projection(projection) => {
                let items: Vec<&str> = projection.projection.iter().map(|item| item.output_name.as_str()).collect();
                format!("Projection({})", items.join(", "))
            }
            LogicalPlan::Distinct(_) => "Distinct".to_string(),
            LogicalPlan::Sort(sort) => {
                let keys: Vec<String> = sort
                    .keys
                    .iter()
                    .map(|key| {
                        let dir = if key.desc { "DESC" } else { "ASC" };
                        format!("{} {dir}", fmt_bound_expr(&key.expr))
                    })
                    .collect();
                format!("Sort({})", keys.join(", "))
            }
            LogicalPlan::Limit(limit) => match (limit.limit, limit.offset) {
                (Some(n), Some(o)) => format!("Limit(limit={n}, offset={o})"),
                (Some(n), None) => format!("Limit(limit={n})"),
                (None, Some(o)) => format!("Limit(offset={o})"),
                (None, None) => "Limit".to_string(),
            },
            LogicalPlan::Insert(insert) => format!("Insert({})", insert.table_name),
            LogicalPlan::Update(update) => format!("Update({})", update.table_name),
            LogicalPlan::Delete(delete) => format!("Delete({})", delete.table_name),
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

impl fmt::Display for LogicalPlan {
    /// 演算子の木を、根から葉へインデントを深くしながら表示する。
    ///
    /// ```text
    /// Projection(name)
    ///   └─ Filter(id = 42)
    ///     └─ Scan(users)
    /// ```
    ///
    /// この表示は`EXPLAIN`そのものではない。実行アルゴリズムを持たない
    /// `LogicalPlan`をそのまま覗き見るための表現であり、`Scan`が実際に
    /// 全件走査になるか索引を使うかのような`Physical Plan`の情報は含まない
    /// (`EXPLAIN`の実装は第19章)。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_tree(f, 0)
    }
}

/// `Projection`が生成する行の`Schema`を、入力側の`Schema`と束縛済みの
/// 射影対象リストから組み立てる。
///
/// 単純な列参照(`BoundExpr::ColumnRef`)は、入力側の列定義(型・nullable)を
/// そのまま引き継ぐ。計算結果の式(`id + 1`など)は、`item.expr.data_type()`
/// (`Binder`が構築時に決めた型)を使い、`None`(型が定まらない`NULL`単体など)
/// は`TEXT`で代用する。この場合の`nullable`は、行ごとに`NULL`になったり
/// ならなかったりしうるため常に`true`にする。
///
/// `executor::project`が実際に行を計算するときも、列構成の決め方はこの関数と
/// 同じ規則に従う(`executor::project`はこの関数を呼ぶ)。列構成の決め方が
/// 2箇所に分かれてずれる事態を避けるため、実装は1箇所(ここ)にしかない。
pub fn projection_schema(input_schema: &Schema, projection: &[BoundSelectItem]) -> Schema {
    let mut out_columns = Vec::with_capacity(projection.len());
    for item in projection {
        if let BoundExpr::ColumnRef { column_index, .. } = &item.expr {
            let mut column = input_schema.columns()[*column_index].clone();
            column.name = item.output_name.clone();
            out_columns.push(column);
            continue;
        }

        let data_type = item.expr.data_type().unwrap_or(DataType::Text);
        out_columns.push(Column::new(item.output_name.clone(), data_type, true));
    }
    Schema::new(out_columns)
}

/// `Join`が生成する行の`Schema`を、左右の`Schema`から組み立てる。
/// 単純に列を連結するだけであり、同名の列が両側にあってもここでは
/// 衝突を検査しない(重複した列名を許すのはSQLの中間結果として自然な
/// ことであり、`Binder`は`column_index`を使うぶんこの重複を気にしない。
/// 利用者が最終的に見る出力列名は、常にこの`Schema`を直接ではなく
/// `SELECT`の`projection`(`Binder`が展開・解決済み)から決まる)。
pub fn join_schema(left: &Schema, right: &Schema) -> Schema {
    let mut columns = Vec::with_capacity(left.len() + right.len());
    columns.extend(left.columns().iter().cloned());
    columns.extend(right.columns().iter().cloned());
    Schema::new(columns)
}

/// `BoundSelect::tables`・`joins`から`FROM`部分の木を組み立てる。
///
/// `tables`が空なら(`FROM`を伴わない`SELECT`)列を持たない`Values`を1件返す。
/// そうでなければ、先頭のテーブルを`Scan`にし、以降のテーブルを`joins`の
/// 対応する`ON`条件で1つずつ`Join`として重ねていく。`n`個の`JOIN`から
/// できる木は、`((table0 JOIN table1) JOIN table2) JOIN ...`という左深い木
/// (left-deep tree)になる。`tables[i]`と`joins[i - 1]`が対応する
/// (`Binder::bind_from`のドキュメント参照)。
fn build_from(tables: Vec<crate::binder::BoundTableRef>, joins: Vec<crate::binder::BoundJoinStep>) -> LogicalPlan {
    let mut tables = tables.into_iter();
    let Some(first) = tables.next() else {
        return LogicalPlan::Values(ValuesNode { schema: Schema::new(Vec::new()), rows: vec![Vec::new()] });
    };

    let mut plan = LogicalPlan::Scan(ScanNode { table_id: first.table_id, table_name: first.table_name, schema: first.schema });
    for (table, join) in tables.zip(joins) {
        let right = LogicalPlan::Scan(ScanNode { table_id: table.table_id, table_name: table.table_name, schema: table.schema });
        plan = LogicalPlan::Join(JoinNode {
            left: Box::new(plan),
            right: Box::new(right),
            kind: join.kind,
            condition: join.condition,
        });
    }
    plan
}

/// `BoundStatement::Select`を`LogicalPlan`へ変換する。
///
/// `FROM`があれば`Scan`(`JOIN`があれば`Join`を重ねた左深い木)を根にし、
/// 無ければ列を持たない行を1件生成する`Values`を根にする。どちらの場合も、
/// `WHERE`があれば`Filter`を、最後に必ず`Projection`を積む。
///
/// ```text
/// SELECT name FROM users WHERE id = 42
///   Projection(name)
///     └─ Filter(id = 42)
///       └─ Scan(users)
///
/// SELECT 1 + 1
///   Projection(1 + 1)
///     └─ Values(1 row)
/// ```
/// `BoundStatement::Select`を`LogicalPlan`へ変換する。
///
/// 演算子は`Filter(WHERE) → Aggregate → Filter(HAVING) → Projection → Distinct
/// → Sort → Limit`という、標準SQLの評価順序(モジュール冒頭の説明を参照)と
/// 同じ並びで積む。`GROUP BY`も集約関数も無い`SELECT`(`select.aggregate`が
/// `None`)では`Aggregate`と`HAVING`のFilterを飛ばし、`DISTINCT`・`ORDER BY`・
/// `LIMIT`/`OFFSET`を伴わない`SELECT`では対応するノードをそもそも積まない。
/// 第18章までの実装が「`Filter`の後に必ず`Projection`が続く」という固定の
/// 2段構成だったのに対し、この章では`select`が持つ情報の有無に応じて木の
/// 深さそのものが変わる。
pub fn build_select(select: BoundSelect) -> LogicalPlan {
    let source = build_from(select.tables, select.joins);

    let filtered = match select.predicate {
        Some(predicate) => LogicalPlan::Filter(FilterNode { input: Box::new(source), predicate }),
        None => source,
    };

    let after_having = match select.aggregate {
        Some(aggregate) => {
            let aggregated = LogicalPlan::Aggregate(AggregateNode {
                input: Box::new(filtered),
                group_by: aggregate.group_by,
                calls: aggregate.calls,
                schema: aggregate.schema,
            });
            match select.having {
                Some(having) => LogicalPlan::Filter(FilterNode { input: Box::new(aggregated), predicate: having }),
                None => aggregated,
            }
        }
        None => filtered,
    };

    let visible_len = select.projection.len() - select.hidden_column_count;
    let projected =
        LogicalPlan::Projection(ProjectionNode { input: Box::new(after_having), projection: select.projection });

    let distinct = if select.distinct { LogicalPlan::Distinct(DistinctNode { input: Box::new(projected) }) } else { projected };

    let sorted = if select.order_by.is_empty() {
        distinct
    } else {
        let keys = select.order_by.into_iter().map(|item| SortKey { expr: item.expr, desc: item.desc }).collect();
        LogicalPlan::Sort(SortNode { input: Box::new(distinct), keys })
    };

    let limited = if select.limit.is_none() && select.offset.is_none() {
        sorted
    } else {
        LogicalPlan::Limit(LimitNode { input: Box::new(sorted), limit: select.limit, offset: select.offset })
    };

    // `ORDER BY`が`SELECT`の対象式に無い式を参照した場合(`Binder::bind_select`の
    // 「`ORDER BY`はどの範囲を束縛するか」を参照)、その式は`projected`の末尾に
    // **隠し列**として積まれている。`Sort`まではこの隠し列が必要だが、最終的に
    // 利用者へ返す行には含めない。ここで先頭`visible_len`列だけを残す
    // トリム用の`Projection`をもう1段積むことで、隠し列を取り除く。
    // `Distinct`より後にトリムしているのは、`hidden_column_count > 0`のとき
    // `Binder`が`DISTINCT`と隠し列の組み合わせをすでに拒否しているため
    // (この経路には到達しない)ではなく、単に「`Sort`・`Limit`が終わってから
    // 落とす」という順序が最も無駄が無いからである(`Limit`が件数を絞った後の
    // 行だけをトリムすればよい)。
    if select.hidden_column_count == 0 {
        limited
    } else {
        let extended_schema = limited.output_schema();
        let trim_projection = (0..visible_len)
            .map(|index| {
                let column = &extended_schema.columns()[index];
                BoundSelectItem {
                    expr: BoundExpr::ColumnRef {
                        table_ordinal: 0,
                        column_index: index,
                        name: column.name.clone(),
                        data_type: column.data_type,
                        span: select.span,
                    },
                    output_name: column.name.clone(),
                }
            })
            .collect();
        LogicalPlan::Projection(ProjectionNode { input: Box::new(limited), projection: trim_projection })
    }
}

/// `BoundStatement::Insert`を`LogicalPlan`へ変換する。
///
/// `VALUES`の各行は、まだ評価していない`Expr`のまま`Values`ノードへ積む。
/// `Insert`はその`Values`を子に持つ。
pub fn build_insert(insert: BoundInsert) -> LogicalPlan {
    let values = LogicalPlan::Values(ValuesNode { schema: insert.schema.clone(), rows: insert.rows });
    LogicalPlan::Insert(InsertNode {
        table_id: insert.table_id,
        table_name: insert.table_name,
        schema: insert.schema,
        columns: insert.columns,
        input: Box::new(values),
    })
}

/// `BoundStatement::Update`を`LogicalPlan`へ変換する。
///
/// `input`は書き換え対象のテーブルを表す`Scan`である。この章の実行経路
/// (`Database::execute_update`)は、`Scan`を実際に材質化してから`Filter`・
/// `Update`を別々に適用するのではなく、`executor::update`(第17章)が
/// 「走査しながら`predicate`を評価し、一致した行だけ書き換える」という
/// 1回の走査にまとめて行う。それでも`input`を`Scan`として木に残しているのは、
/// `Update`が「どのテーブルに対する操作か」を演算子の親子関係として表現する
/// ためであり、実行方法(1回の走査にまとめるか、`Filter`を独立させるか)は
/// 第19章のPhysical Planが決める領域だからである。
pub fn build_update(update: BoundUpdate) -> LogicalPlan {
    let scan = LogicalPlan::Scan(ScanNode {
        table_id: update.table_id,
        table_name: update.table_name.clone(),
        schema: update.schema.clone(),
    });
    LogicalPlan::Update(UpdateNode {
        table_id: update.table_id,
        table_name: update.table_name,
        schema: update.schema,
        assignments: update.assignments,
        predicate: update.predicate,
        input: Box::new(scan),
    })
}

/// `BoundStatement::Delete`を`LogicalPlan`へ変換する。`build_update`と同じ理由で、
/// `input`は書き換え(削除)対象のテーブルを表す`Scan`である。
pub fn build_delete(delete: BoundDelete) -> LogicalPlan {
    let scan = LogicalPlan::Scan(ScanNode {
        table_id: delete.table_id,
        table_name: delete.table_name.clone(),
        schema: delete.schema.clone(),
    });
    LogicalPlan::Delete(DeleteNode {
        table_id: delete.table_id,
        table_name: delete.table_name,
        schema: delete.schema,
        predicate: delete.predicate,
        input: Box::new(scan),
    })
}

/// `BoundExpr`を、木の表示(`LogicalPlan`の`Display`実装)のためだけに
/// 人が読める形の文字列へ変換する。`Binder`が保持する元のソース文字列の
/// 範囲(`Span`)を経由せず、式の構造から組み立て直す。そのため、
/// `SELECT id+1`と書いても`id + 1`のように空白の入り方が変わることがある。
/// SQLへ逆変換する用途(prepared statementのログ出力など)には使わない。
pub(crate) fn fmt_bound_expr(expr: &BoundExpr) -> String {
    match expr {
        BoundExpr::IntLiteral { value, .. } => value.to_string(),
        BoundExpr::StringLiteral { value, .. } => format!("'{value}'"),
        BoundExpr::BoolLiteral { value, .. } => value.to_string(),
        BoundExpr::NullLiteral { .. } => "NULL".to_string(),
        BoundExpr::ColumnRef { name, .. } => name.clone(),
        BoundExpr::UnaryOp { op, expr, .. } => match op {
            UnaryOperator::Negate => format!("-{}", fmt_bound_expr(expr)),
            UnaryOperator::Not => format!("NOT {}", fmt_bound_expr(expr)),
        },
        BoundExpr::BinaryOp { op, lhs, rhs, .. } => {
            format!("{} {} {}", fmt_bound_expr(lhs), fmt_binary_operator(*op), fmt_bound_expr(rhs))
        }
        BoundExpr::IsNull { expr, negated, .. } => {
            let suffix = if *negated { "IS NOT NULL" } else { "IS NULL" };
            format!("{} {suffix}", fmt_bound_expr(expr))
        }
        BoundExpr::FunctionCall { name, args, .. } => {
            let args: Vec<String> = args.iter().map(fmt_bound_expr).collect();
            format!("{name}({})", args.join(", "))
        }
        BoundExpr::Aggregate { func, arg, .. } => {
            let arg = arg.as_deref().map(fmt_bound_expr).unwrap_or_else(|| "*".to_string());
            format!("{}({arg})", func.name())
        }
        BoundExpr::Paren { expr, .. } => format!("({})", fmt_bound_expr(expr)),
        BoundExpr::Cast { expr, data_type, .. } => format!("CAST({} AS {data_type})", fmt_bound_expr(expr)),
    }
}

/// [`AggregateCall`]を`COUNT(*)`、`SUM(price)`のような文字列にする。
/// `LogicalPlan`の`Display`実装(`EXPLAIN`の`Aggregate`ノード)に加えて、
/// `Binder::rewrite_for_aggregate`(第21章)が同じ呼び出しかどうかを判定する
/// ためにも使う(`pub(crate)`にしている理由)。
pub(crate) fn fmt_aggregate_call(call: &AggregateCall) -> String {
    let arg = call.arg.as_deref().map(fmt_bound_expr).unwrap_or_else(|| "*".to_string());
    format!("{}({arg})", call.func.name())
}

fn fmt_binary_operator(op: BinaryOperator) -> &'static str {
    match op {
        BinaryOperator::Add => "+",
        BinaryOperator::Subtract => "-",
        BinaryOperator::Multiply => "*",
        BinaryOperator::Divide => "/",
        BinaryOperator::Eq => "=",
        BinaryOperator::NotEq => "<>",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        BinaryOperator::And => "AND",
        BinaryOperator::Or => "OR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binder::Binder;
    use crate::catalog::Catalog;
    use crate::eval::FunctionRegistry;
    use crate::parser::parse_statement;
    use crate::types::Column;

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

    fn bind(sql: &str, catalog: &Catalog) -> crate::binder::BoundStatement {
        let statement = parse_statement(sql).unwrap();
        let functions = FunctionRegistry::with_builtins();
        Binder::new(catalog, &functions, sql).bind(statement).unwrap()
    }

    #[test]
    fn select_with_from_and_where_builds_projection_over_filter_over_scan() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Select(select) = bind("SELECT name FROM users WHERE id = 42", &catalog)
        else {
            panic!("Selectを期待した");
        };
        let plan = build_select(*select);
        assert_eq!(
            plan.to_string(),
            "Projection(name)\n  └─ Filter(id = 42)\n    └─ Scan(users)\n"
        );
    }

    #[test]
    fn select_without_from_builds_projection_over_values() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Select(select) = bind("SELECT 1 + 1", &catalog) else {
            panic!("Selectを期待した");
        };
        let plan = build_select(*select);
        assert_eq!(plan.to_string(), "Projection(1 + 1)\n  └─ Values(1 row)\n");
    }

    #[test]
    fn select_without_where_skips_filter_node() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Select(select) = bind("SELECT id FROM users", &catalog) else {
            panic!("Selectを期待した");
        };
        let plan = build_select(*select);
        assert_eq!(plan.to_string(), "Projection(id)\n  └─ Scan(users)\n");
    }

    #[test]
    fn select_output_schema_matches_projected_columns() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Select(select) = bind("SELECT id, name FROM users", &catalog) else {
            panic!("Selectを期待した");
        };
        let plan = build_select(*select);
        let schema = plan.output_schema();
        let names: Vec<&str> = schema.columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["id", "name"]);
        assert_eq!(schema.columns()[0].data_type, DataType::BigInt);
        assert!(!schema.columns()[0].nullable);
    }

    #[test]
    fn insert_builds_insert_over_values() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Insert(insert) =
            bind("INSERT INTO users (id, name) VALUES (1, 'a')", &catalog)
        else {
            panic!("Insertを期待した");
        };
        let plan = build_insert(insert);
        assert_eq!(plan.to_string(), "Insert(users)\n  └─ Values(1 row)\n");
        assert!(plan.output_schema().is_empty());
    }

    #[test]
    fn update_builds_update_over_scan() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Update(update) =
            bind("UPDATE users SET name = 'x' WHERE id = 1", &catalog)
        else {
            panic!("Updateを期待した");
        };
        let plan = build_update(update);
        assert_eq!(plan.to_string(), "Update(users)\n  └─ Scan(users)\n");
    }

    #[test]
    fn delete_builds_delete_over_scan() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Delete(delete) = bind("DELETE FROM users WHERE id = 1", &catalog) else {
            panic!("Deleteを期待した");
        };
        let plan = build_delete(delete);
        assert_eq!(plan.to_string(), "Delete(users)\n  └─ Scan(users)\n");
    }
}
