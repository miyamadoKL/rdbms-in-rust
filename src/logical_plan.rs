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
//! 演算子は次の7種類にとどめる。`Scan`・`Values`・`Filter`・`Projection`は
//! `SELECT`が、`Insert`・`Update`・`Delete`はそれぞれの文が使う。`Join`・
//! `Aggregate`・`Sort`・`Limit`にあたる構文はまだこのクレートに無いため、
//! この章では対応する演算子を作らない(第21・22章で`LogicalPlan`にバリアントを
//! 追加する余地として残す)。
//!
//! `LogicalPlan`が確定させるのは「何を計算するか」という演算子の並びと
//! 依存関係だけであり、「どう計算するか」(`Scan`が全件走査になるのか索引を
//! 使うのか)は決めない。索引はまだこのクレートに無いので、この区別は
//! 第23〜25章でB+TreeとIndex Scanが揃うまでは意味を持たないが、`Scan`という
//! 名前は最初から「走査する対象」だけを表し、「どう走査するか」を含まない
//! 名前として選んである。

use std::fmt;

use crate::ast::{BinaryOperator, Expr, UnaryOperator};
use crate::binder::{
    BoundAssignment, BoundDelete, BoundExpr, BoundInsert, BoundSelect, BoundSelectItem, BoundUpdate,
};
use crate::ids::TableId;
use crate::types::{Column, DataType, Schema};

/// 関係代数の演算子1個。
///
/// `Filter`・`Projection`・`Insert`・`Update`・`Delete`は、それぞれ1個の
/// 子(`input`)を持つ。`Scan`・`Values`は子を持たない葉である。
#[derive(Debug, Clone, PartialEq)]
pub enum LogicalPlan {
    /// テーブル全体を走査する。
    Scan(ScanNode),
    /// `VALUES`が並べる、リテラル式の行の並び。`FROM`を伴わない`SELECT`は、
    /// 列を1つも持たない行を1件だけ持つ`Values`を入力とみなす。
    Values(ValuesNode),
    /// `predicate`が`TRUE`になった行だけを残す。
    Filter(FilterNode),
    /// 各行から`projection`が指す列・式だけを取り出す。
    Projection(ProjectionNode),
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

/// [`LogicalPlan::Projection`]が持つ情報。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionNode {
    pub input: Box<LogicalPlan>,
    pub projection: Vec<BoundSelectItem>,
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
            LogicalPlan::Projection(projection) => {
                projection_schema(&projection.input.output_schema(), &projection.projection)
            }
            LogicalPlan::Insert(_) | LogicalPlan::Update(_) | LogicalPlan::Delete(_) => Schema::new(Vec::new()),
        }
    }

    /// この演算子が直接持つ子。`Scan`・`Values`は葉なので空を返す。
    fn children(&self) -> Vec<&LogicalPlan> {
        match self {
            LogicalPlan::Scan(_) | LogicalPlan::Values(_) => Vec::new(),
            LogicalPlan::Filter(filter) => vec![&filter.input],
            LogicalPlan::Projection(projection) => vec![&projection.input],
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
            LogicalPlan::Projection(projection) => {
                let items: Vec<&str> = projection.projection.iter().map(|item| item.output_name.as_str()).collect();
                format!("Projection({})", items.join(", "))
            }
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

/// `BoundStatement::Select`を`LogicalPlan`へ変換する。
///
/// `FROM`があれば`Scan`を根にし、無ければ列を持たない行を1件生成する
/// `Values`を根にする。どちらの場合も、`WHERE`があれば`Filter`を、最後に
/// 必ず`Projection`を積む。
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
pub fn build_select(select: BoundSelect) -> LogicalPlan {
    let source = match select.tables.into_iter().next() {
        Some(table) => LogicalPlan::Scan(ScanNode {
            table_id: table.table_id,
            table_name: table.table_name,
            schema: table.schema,
        }),
        None => LogicalPlan::Values(ValuesNode {
            schema: Schema::new(Vec::new()),
            rows: vec![Vec::new()],
        }),
    };

    let filtered = match select.predicate {
        Some(predicate) => LogicalPlan::Filter(FilterNode { input: Box::new(source), predicate }),
        None => source,
    };

    LogicalPlan::Projection(ProjectionNode { input: Box::new(filtered), projection: select.projection })
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
fn fmt_bound_expr(expr: &BoundExpr) -> String {
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
        BoundExpr::Paren { expr, .. } => format!("({})", fmt_bound_expr(expr)),
        BoundExpr::Cast { expr, data_type, .. } => format!("CAST({} AS {data_type})", fmt_bound_expr(expr)),
    }
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
        let plan = build_select(select);
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
        let plan = build_select(select);
        assert_eq!(plan.to_string(), "Projection(1 + 1)\n  └─ Values(1 row)\n");
    }

    #[test]
    fn select_without_where_skips_filter_node() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Select(select) = bind("SELECT id FROM users", &catalog) else {
            panic!("Selectを期待した");
        };
        let plan = build_select(select);
        assert_eq!(plan.to_string(), "Projection(id)\n  └─ Scan(users)\n");
    }

    #[test]
    fn select_output_schema_matches_projected_columns() {
        let catalog = users_catalog();
        let crate::binder::BoundStatement::Select(select) = bind("SELECT id, name FROM users", &catalog) else {
            panic!("Selectを期待した");
        };
        let plan = build_select(select);
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
