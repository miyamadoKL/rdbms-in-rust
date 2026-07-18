//! ASTをBound ASTへ変換する`Binder`(名前解決層)。
//!
//! `Parser`(第7章)が組み立てる`Statement`/`Expr`は、`users`や`id`が実在する
//! テーブル・列を指すかどうかを一切見ない、構文だけを確定させた中間表現だった。
//! この章の`Binder`は、そのASTをカタログ(`Catalog`または`Storage`。どちらも
//! `CatalogLookup`として抽象化する)と突き合わせ、次を行う。
//!
//! * **テーブル名解決**: `FROM users`の`users`をカタログから引き、`TableId`と
//!   `Schema`を持つ[`BoundTableRef`]にする。存在しなければ位置情報付きの
//!   `DbError::Bind`にする。
//! * **列名解決**: `Expr::ColumnRef`を、テーブルの並び上の順序(`table_ordinal`)
//!   と列の索引(`column_index`)、型を持つ[`BoundExpr::ColumnRef`]にする。
//! * **`*`の展開**: `SELECT *`は、束縛の時点で`FROM`先の具体的な列参照の並びに
//!   置き換える。Bound ASTに`*`という記法自体は残らない。
//! * **Alias**: `FROM users AS u`の`u`を、以後の列参照の修飾子として使えるように
//!   する。`u.id`は`ColumnRef { qualifier: Some(u), name: id }`としてASTに現れ、
//!   [`Binder::resolve_column`]がそれを`u`というAliasを持つテーブルの列として解決する。
//! * **曖昧な列名の検出**: 修飾子を伴わない列参照が、`FROM`に並ぶ複数のテーブルの
//!   両方に存在する場合はエラーにする。
//! * **式の型検査**: 第10章で`executor::infer_type`/`check_predicate_type`が
//!   担っていた検査をここへ統合する(モジュール末尾の「型検査をここへ移した理由」
//!   参照)。
//!
//! `Aggregate`(`COUNT`、`SUM`等)の使用位置の検査(`SELECT`の対象式でのみ許す、
//! `GROUP BY`の無い列との共存を禁じる、など)は、`Aggregate`自体が第21章まで
//! 実装されないため、この章では行わない。
//!
//! # 型検査をここへ移した理由
//!
//! 第10章の`infer_type`/`check_predicate_type`は、`executor`モジュールに間借り
//! する形で実装されていた。これは、当時はまだ「実行の前段」という層が存在せず、
//! `Database::execute`が`executor`の演算子を直接呼ぶだけの構造だったからである。
//! しかし`infer_type`が実際にしていたことは、式の評価(`Value`を計算すること)
//! ではなく、式の**意味**(列参照が指す列、演算子が要求する型)を決めることであり、
//! これは名前解決と同じ層の仕事である。`executor`に残したままだと、列参照の
//! 解決(`schema.column(name)`)と型検査(`infer_type`)が、`Binder`が新設する
//! 列インデックス・型付きの`BoundExpr`と二重に、しかも別々の場所で行われることに
//! なる。この章で`infer_type`と`check_predicate_type`を`Binder`へ統合し、
//! `executor`からは削除した。`executor`の各演算子(`filter`・`project`)は、
//! 束縛済みで型検査済みの`BoundExpr`だけを受け取るようになり、二重検査は無くなる。
//!
//! `executor::predicate_matches`(`WHERE`の評価結果を`bool`へ変換する関数)だけは、
//! 型を問わない`BOOLEAN`/`NULL`以外の値に出会った場合の分岐を残してある。
//! これは`Binder`を経由しない呼び出し経路を想定した保険ではなく(`Database::execute`
//! は常に`Binder`を経由する)、`BoundExpr::data_type()`が`Some(Boolean)`または
//! `None`であることをコンパイラは保証しないという、`Rust`の型システム上の限界に
//! 対する最終防衛線である。`Binder`が誤って型検査を素通りさせた場合(将来の
//! バグ)でも、`executor`が`BOOLEAN`でない値を暗黙に「マッチしない」側へ丸めて
//! しまうことだけは避けたい、という意図を残すためにこの分岐だけは消していない。

use std::collections::HashSet;

use crate::ast::{
    Assignment, BinaryOperator, CreateTableStatement, DeleteStatement, DropTableStatement, Expr,
    FromClause, Ident, InsertStatement, SelectItem, SelectStatement, Statement, UnaryOperator,
    UpdateStatement,
};
use crate::catalog::{Catalog, TableInfo};
use crate::error::{DbError, DbResult};
use crate::eval::FunctionRegistry;
use crate::ids::TableId;
use crate::lexer::{self, Span};
use crate::storage::Storage;
use crate::types::{DataType, Schema};

/// テーブル名から[`TableInfo`]を引ける、カタログの抽象。
///
/// `Database`(第16章)がすでに`Backend::Memory`(`Catalog`)と`Backend::Disk`
/// (`Storage`)という2つのテーブル定義の持ち方を使い分けている。`Binder`は
/// どちらの持ち方かを意識する必要が無いので、この2つを1つのtraitとして
/// 抽象化する。第16章の`Database::table_info`が`match`で吸収していた分岐を、
/// この章では型の側(trait)へ移した形になる。
pub trait CatalogLookup {
    /// テーブル名から`TableInfo`を引く。見つからなければ`None`を返す。
    fn table(&self, name: &str) -> Option<&TableInfo>;
}

impl CatalogLookup for Catalog {
    fn table(&self, name: &str) -> Option<&TableInfo> {
        Catalog::table(self, name)
    }
}

impl CatalogLookup for Storage {
    fn table(&self, name: &str) -> Option<&TableInfo> {
        Storage::table(self, name)
    }
}

/// 名前解決・型検査を終えた文。
///
/// `CreateTable`だけはASTのバリアントをそのまま持ち回す。`CREATE TABLE`が
/// 定義するのは既存の名前ではなく新しい名前であり、突き合わせるべき既存の
/// カタログエントリが無い(列の型名(`BIGINT`等)の解決は、名前解決ではなく
/// `Schema`の組み立てそのものなので、引き続き`Database::execute_create_table`
/// が担う)。`DropTable`はテーブルの存在をここで確認するが、実行(カタログからの
/// 削除)は名前ベースのままでよいため、ASTのバリアントをそのまま返す。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundStatement {
    Select(BoundSelect),
    CreateTable(CreateTableStatement),
    DropTable(DropTableStatement),
    Insert(BoundInsert),
    Update(BoundUpdate),
    Delete(BoundDelete),
}

/// `FROM`(または`INSERT INTO`・`UPDATE`・`DELETE FROM`)が指す1テーブル。
///
/// `schema`は束縛の時点でのカタログの内容を複製したものであり、以後の
/// `BoundExpr::ColumnRef`の`column_index`はこの`schema`の列の並びに対応する。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundTableRef {
    pub table_id: TableId,
    pub table_name: String,
    /// `AS`で指定されたAlias。`None`なら`table_name`がそのまま修飾子になる。
    pub alias: Option<String>,
    pub schema: Schema,
}

impl BoundTableRef {
    /// この表を指す修飾子。`u.id`の`u`のように、列参照の`qualifier`と比較する
    /// ときに使う(Aliasがあれば優先し、無ければテーブル名そのもの)。
    pub fn qualifier(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.table_name)
    }
}

/// 束縛済みの`SELECT`。
///
/// `tables`は`FROM`が無ければ0個、あれば1個の`Vec`になる。要素数を`1`に固定
/// した型(`Option<BoundTableRef>`)ではなく`Vec`にしているのは、第22章の
/// `JOIN`で複数テーブルの`FROM`が導入されたときに、この型をそのまま使い
/// 回せるようにするためである。[`Binder::resolve_column`]の曖昧列検出も、
/// この`Vec`の要素数に関係なく動く形で書いてある。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    pub tables: Vec<BoundTableRef>,
    pub projection: Vec<BoundSelectItem>,
    pub predicate: Option<BoundExpr>,
    pub span: Span,
}

/// 束縛済みの射影対象1個。`*`はここに来る前に個々の列参照へ展開済みなので、
/// `output_name`はワイルドカードかどうかに関係なく必ず1つの列名を持つ。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelectItem {
    pub expr: BoundExpr,
    pub output_name: String,
}

/// 名前解決・型検査を終えた式。
///
/// `Expr`との違いは2つ。列参照(`ColumnRef`)が名前ではなく
/// `(table_ordinal, column_index)`という解決済みの座標を持つこと、そして
/// 演算子・関数呼び出しのノードが自分自身の出力の型(`data_type`)を持つことである。
/// 型は`bind_expr`が構築時に一度だけ計算し、以後(`executor`での評価)は
/// 再計算しない。
#[derive(Debug, Clone, PartialEq)]
pub enum BoundExpr {
    IntLiteral {
        value: i64,
        span: Span,
    },
    StringLiteral {
        value: String,
        span: Span,
    },
    BoolLiteral {
        value: bool,
        span: Span,
    },
    NullLiteral {
        span: Span,
    },
    /// 列参照。`table_ordinal`は所属する[`BoundSelect::tables`]の添字、
    /// `column_index`はそのテーブルの`Schema`上の列の添字。
    ColumnRef {
        table_ordinal: usize,
        column_index: usize,
        name: String,
        data_type: DataType,
        span: Span,
    },
    UnaryOp {
        op: UnaryOperator,
        expr: Box<BoundExpr>,
        data_type: DataType,
        span: Span,
    },
    BinaryOp {
        op: BinaryOperator,
        lhs: Box<BoundExpr>,
        rhs: Box<BoundExpr>,
        data_type: DataType,
        span: Span,
    },
    IsNull {
        expr: Box<BoundExpr>,
        negated: bool,
        span: Span,
    },
    FunctionCall {
        name: String,
        args: Vec<BoundExpr>,
        data_type: DataType,
        span: Span,
    },
    Paren {
        expr: Box<BoundExpr>,
        span: Span,
    },
    Cast {
        expr: Box<BoundExpr>,
        data_type: DataType,
        span: Span,
    },
}

impl BoundExpr {
    /// この式が返す値の型。`None`は「型が定まらない」ことを表し、`NullLiteral`
    /// と、それを素通しする`Paren`の入れ子だけがこれに当たる。`executor::infer_type`
    /// (第10章)が返していた`Option<DataType>`と同じ規則で、`Value::data_type()`が
    /// `Value::Null`に対して`None`を返すのと対応する。
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            BoundExpr::IntLiteral { .. } => Some(DataType::BigInt),
            BoundExpr::StringLiteral { .. } => Some(DataType::Text),
            BoundExpr::BoolLiteral { .. } => Some(DataType::Boolean),
            BoundExpr::NullLiteral { .. } => None,
            BoundExpr::ColumnRef { data_type, .. }
            | BoundExpr::UnaryOp { data_type, .. }
            | BoundExpr::BinaryOp { data_type, .. }
            | BoundExpr::FunctionCall { data_type, .. }
            | BoundExpr::Cast { data_type, .. } => Some(*data_type),
            BoundExpr::IsNull { .. } => Some(DataType::Boolean),
            BoundExpr::Paren { expr, .. } => expr.data_type(),
        }
    }

    /// この式がソース中で占める範囲。
    pub fn span(&self) -> Span {
        match self {
            BoundExpr::IntLiteral { span, .. }
            | BoundExpr::StringLiteral { span, .. }
            | BoundExpr::BoolLiteral { span, .. }
            | BoundExpr::NullLiteral { span }
            | BoundExpr::ColumnRef { span, .. }
            | BoundExpr::UnaryOp { span, .. }
            | BoundExpr::BinaryOp { span, .. }
            | BoundExpr::IsNull { span, .. }
            | BoundExpr::FunctionCall { span, .. }
            | BoundExpr::Paren { span, .. }
            | BoundExpr::Cast { span, .. } => *span,
        }
    }
}

/// `UPDATE`の`SET`リストに並ぶ、束縛済みの代入1個。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundAssignment {
    pub column_index: usize,
    pub value: BoundExpr,
}

/// 束縛済みの`INSERT INTO`。
///
/// `VALUES`の各行(`rows`)はASTの`Expr`のまま残す。`VALUES`は既存の行を参照する
/// 構文を持たない(`eval_expr`に渡す`row`は常に`None`)ため、列参照は現れようが
/// なく、`Binder`が解決すべき名前は無い。列参照が構文として書けてしまった場合
/// (`INSERT INTO t VALUES (id)`)は、第10章までと同じく実行時の`DbError::Eval`
/// に委ねる。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundInsert {
    pub table_id: TableId,
    pub schema: Schema,
    /// 明示された列名を、`schema`上の列インデックスへ解決した並び。`None`なら
    /// `VALUES`の並びをそのままスキーマの列順とみなす。
    pub columns: Option<Vec<usize>>,
    pub rows: Vec<Vec<Expr>>,
    pub span: Span,
}

/// 束縛済みの`UPDATE`。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundUpdate {
    pub table_id: TableId,
    pub schema: Schema,
    pub assignments: Vec<BoundAssignment>,
    pub predicate: Option<BoundExpr>,
    pub span: Span,
}

/// 束縛済みの`DELETE FROM`。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundDelete {
    pub table_id: TableId,
    pub schema: Schema,
    pub predicate: Option<BoundExpr>,
    pub span: Span,
}

/// `sql`(元のSQL文字列。エラー位置の行・列を計算するために使う)を経由して、
/// `catalog`・`functions`と1本のASTを結び付ける。
pub struct Binder<'a> {
    catalog: &'a dyn CatalogLookup,
    functions: &'a FunctionRegistry,
    sql: &'a str,
}

impl<'a> Binder<'a> {
    pub fn new(catalog: &'a dyn CatalogLookup, functions: &'a FunctionRegistry, sql: &'a str) -> Self {
        Binder { catalog, functions, sql }
    }

    /// AST(`Statement`)をBound AST(`BoundStatement`)へ変換する。
    pub fn bind(&self, statement: Statement) -> DbResult<BoundStatement> {
        match statement {
            Statement::Select(select) => self.bind_select(select).map(BoundStatement::Select),
            Statement::CreateTable(create) => Ok(BoundStatement::CreateTable(create)),
            Statement::DropTable(drop) => self.bind_drop_table(drop),
            Statement::Insert(insert) => self.bind_insert(insert).map(BoundStatement::Insert),
            Statement::Update(update) => self.bind_update(update).map(BoundStatement::Update),
            Statement::Delete(delete) => self.bind_delete(delete).map(BoundStatement::Delete),
        }
    }

    fn error_at(&self, span: Span, message: impl Into<String>) -> DbError {
        let (line, column) = lexer::line_col(self.sql, span.start);
        DbError::Bind { message: message.into(), line, column }
    }

    /// テーブル名をカタログと突き合わせ、`BoundTableRef`にする。
    fn resolve_table(&self, table: &Ident, alias: Option<&Ident>) -> DbResult<BoundTableRef> {
        let info = self
            .catalog
            .table(&table.name)
            .ok_or_else(|| self.error_at(table.span, format!("テーブルが見つかりません: {}", table.name)))?;
        Ok(BoundTableRef {
            table_id: info.id,
            table_name: info.name.clone(),
            alias: alias.map(|a| a.name.clone()),
            schema: info.schema.clone(),
        })
    }

    fn bind_drop_table(&self, drop: DropTableStatement) -> DbResult<BoundStatement> {
        // `Catalog::drop_table`・`Storage::drop_table`自身も未知のテーブル名を
        // `DbError::TableNotFound`として検出するが、位置情報を持たない。ここで
        // 先に位置付きの`DbError::Bind`として検出し、実行(`Database::execute_drop_table`)
        // 側は引き続き名前で削除する(束縛結果の`table_id`を使わないのは、削除の
        // 実装自体が第9・15章から名前ベースのままで、変える理由が無いため)。
        self.catalog
            .table(&drop.table.name)
            .ok_or_else(|| self.error_at(drop.table.span, format!("テーブルが見つかりません: {}", drop.table.name)))?;
        Ok(BoundStatement::DropTable(drop))
    }

    fn bind_select(&self, select: SelectStatement) -> DbResult<BoundSelect> {
        let tables = self.bind_from(select.from.as_ref())?;

        let mut projection = Vec::with_capacity(select.items.len());
        for item in select.items {
            match item {
                SelectItem::Wildcard { span } => {
                    if tables.is_empty() {
                        return Err(self.error_at(span, "*はFROMを伴うSELECTでのみ使えます"));
                    }
                    // 複数テーブルの`*`は、テーブルの登場順→各テーブル内は列の
                    // 宣言順に展開する。この章の`Parser`はFROMに1テーブルしか
                    // 持てないため`tables.len()`は常に`1`だが、この展開順序は
                    // 第22章のJOINで複数テーブルになっても変わらない規則として
                    // 先に決めておく。
                    for (table_ordinal, table) in tables.iter().enumerate() {
                        for (column_index, column) in table.schema.columns().iter().enumerate() {
                            projection.push(BoundSelectItem {
                                expr: BoundExpr::ColumnRef {
                                    table_ordinal,
                                    column_index,
                                    name: column.name.clone(),
                                    data_type: column.data_type,
                                    span,
                                },
                                output_name: column.name.clone(),
                            });
                        }
                    }
                }
                SelectItem::Expr { expr, span } => {
                    let output_name = self.sql[span.start..span.end].to_string();
                    let bound = self.bind_expr(&expr, &tables)?;
                    projection.push(BoundSelectItem { expr: bound, output_name });
                }
            }
        }

        let predicate = match &select.where_clause {
            Some(expr) => Some(self.bind_predicate(expr, &tables)?),
            None => None,
        };

        Ok(BoundSelect {
            tables,
            projection,
            predicate,
            span: select.span,
        })
    }

    fn bind_from(&self, from: Option<&FromClause>) -> DbResult<Vec<BoundTableRef>> {
        match from {
            Some(from) => Ok(vec![self.resolve_table(&from.table, from.alias.as_ref())?]),
            None => Ok(Vec::new()),
        }
    }

    fn bind_insert(&self, insert: InsertStatement) -> DbResult<BoundInsert> {
        let table = self.resolve_table(&insert.table, None)?;

        let columns = match &insert.columns {
            Some(cols) => {
                let mut resolved = Vec::with_capacity(cols.len());
                let mut seen = HashSet::with_capacity(cols.len());
                for column in cols {
                    let index = table.schema.index_of(&column.name).ok_or_else(|| {
                        self.error_at(column.span, format!("列'{}'が見つかりません", column.name))
                    })?;
                    if !seen.insert(index) {
                        return Err(self.error_at(
                            column.span,
                            format!("列'{}'がINSERTの列リストに重複しています", column.name),
                        ));
                    }
                    resolved.push(index);
                }
                Some(resolved)
            }
            None => None,
        };

        Ok(BoundInsert {
            table_id: table.table_id,
            schema: table.schema,
            columns,
            rows: insert.rows,
            span: insert.span,
        })
    }

    fn bind_update(&self, update: UpdateStatement) -> DbResult<BoundUpdate> {
        let table = self.resolve_table(&update.table, None)?;
        let tables = std::slice::from_ref(&table);

        let mut assignments = Vec::with_capacity(update.assignments.len());
        for assignment in &update.assignments {
            assignments.push(self.bind_assignment(assignment, tables)?);
        }

        let predicate = match &update.where_clause {
            Some(expr) => Some(self.bind_predicate(expr, tables)?),
            None => None,
        };

        Ok(BoundUpdate {
            table_id: table.table_id,
            schema: table.schema,
            assignments,
            predicate,
            span: update.span,
        })
    }

    fn bind_assignment(&self, assignment: &Assignment, tables: &[BoundTableRef]) -> DbResult<BoundAssignment> {
        let column_index = tables[0].schema.index_of(&assignment.column.name).ok_or_else(|| {
            self.error_at(
                assignment.column.span,
                format!("列'{}'が見つかりません", assignment.column.name),
            )
        })?;
        let value = self.bind_expr(&assignment.value, tables)?;
        Ok(BoundAssignment { column_index, value })
    }

    fn bind_delete(&self, delete: DeleteStatement) -> DbResult<BoundDelete> {
        let table = self.resolve_table(&delete.table, None)?;
        let tables = std::slice::from_ref(&table);

        let predicate = match &delete.where_clause {
            Some(expr) => Some(self.bind_predicate(expr, tables)?),
            None => None,
        };

        Ok(BoundDelete {
            table_id: table.table_id,
            schema: table.schema,
            predicate,
            span: delete.span,
        })
    }

    /// `WHERE`句を束縛したうえで、`BOOLEAN`(または型未定の`NULL`)を返す式に
    /// なっていることを検査する。`executor::check_predicate_type`(第10章)が
    /// 行っていた検査と同じ規則を、束縛の時点でまとめて行う。
    fn bind_predicate(&self, expr: &Expr, tables: &[BoundTableRef]) -> DbResult<BoundExpr> {
        let bound = self.bind_expr(expr, tables)?;
        match bound.data_type() {
            Some(DataType::Boolean) | None => Ok(bound),
            Some(other) => Err(self.error_at(
                bound.span(),
                format!("WHERE句はBOOLEANを返す式である必要があります: 式の型は{other}です"),
            )),
        }
    }

    /// `Expr`を`BoundExpr`へ変換する。列参照の解決(`resolve_column`)に加えて、
    /// 各演算子・関数呼び出しが被演算子に課す型制約を式木全体にわたって再帰的に
    /// 検査する。検査の規則そのものは第10章の`executor::infer_type`をそのまま
    /// 引き継いでいる(被演算子の型が合わなければ`DbError`、出力の型は常に`Some`。
    /// `NullLiteral`とそれを素通しする`Paren`だけが`None`)。
    fn bind_expr(&self, expr: &Expr, tables: &[BoundTableRef]) -> DbResult<BoundExpr> {
        match expr {
            Expr::IntLiteral { value, span } => Ok(BoundExpr::IntLiteral { value: *value, span: *span }),
            Expr::StringLiteral { value, span } => {
                Ok(BoundExpr::StringLiteral { value: value.clone(), span: *span })
            }
            Expr::BoolLiteral { value, span } => Ok(BoundExpr::BoolLiteral { value: *value, span: *span }),
            Expr::NullLiteral { span } => Ok(BoundExpr::NullLiteral { span: *span }),
            Expr::ColumnRef { qualifier, name, span } => {
                self.resolve_column(qualifier.as_ref(), name, *span, tables)
            }
            Expr::Paren { expr, span } => {
                let inner = self.bind_expr(expr, tables)?;
                Ok(BoundExpr::Paren { expr: Box::new(inner), span: *span })
            }
            Expr::UnaryOp { op, expr, span } => {
                let operand = self.bind_expr(expr, tables)?;
                let data_type = self.check_unary_type(*op, &operand, *span)?;
                Ok(BoundExpr::UnaryOp { op: *op, expr: Box::new(operand), data_type, span: *span })
            }
            Expr::BinaryOp { op, lhs, rhs, span } => {
                let bound_lhs = self.bind_expr(lhs, tables)?;
                let bound_rhs = self.bind_expr(rhs, tables)?;
                let data_type = self.check_binary_type(*op, &bound_lhs, &bound_rhs, *span)?;
                Ok(BoundExpr::BinaryOp {
                    op: *op,
                    lhs: Box::new(bound_lhs),
                    rhs: Box::new(bound_rhs),
                    data_type,
                    span: *span,
                })
            }
            Expr::IsNull { expr, negated, span } => {
                // 被演算子の型は問わないが、被演算子自身が無効な式(未知の列参照
                // など)でないことは`bind_expr`の再帰呼び出しが検査する。
                let bound = self.bind_expr(expr, tables)?;
                Ok(BoundExpr::IsNull { expr: Box::new(bound), negated: *negated, span: *span })
            }
            Expr::Cast { expr, type_name, span } => {
                let bound = self.bind_expr(expr, tables)?;
                let data_type = DataType::from_sql_name(&type_name.name).ok_or_else(|| {
                    self.error_at(type_name.span, format!("未知の型名です: {}", type_name.name))
                })?;
                Ok(BoundExpr::Cast { expr: Box::new(bound), data_type, span: *span })
            }
            Expr::FunctionCall { name, args, span } => {
                let arg_types = self.functions.arg_types(name).map_err(|err| self.wrap_eval_error(err, *span))?;
                let canonical_name = name.to_ascii_lowercase();
                if args.len() != arg_types.len() {
                    return Err(self.error_at(
                        *span,
                        format!(
                            "{canonical_name}は引数を{}個取ります(渡されたのは{}個です)",
                            arg_types.len(),
                            args.len()
                        ),
                    ));
                }
                let mut bound_args = Vec::with_capacity(args.len());
                for (arg, expected) in args.iter().zip(arg_types) {
                    let bound_arg = self.bind_expr(arg, tables)?;
                    if let Some(actual) = bound_arg.data_type()
                        && actual != *expected
                    {
                        return Err(self.error_at(
                            bound_arg.span(),
                            format!("{canonical_name}は{expected}を引数に取ります: {actual}が渡されました"),
                        ));
                    }
                    bound_args.push(bound_arg);
                }
                let data_type = self
                    .functions
                    .return_type(name)
                    .map_err(|err| self.wrap_eval_error(err, *span))?;
                Ok(BoundExpr::FunctionCall { name: canonical_name, args: bound_args, data_type, span: *span })
            }
        }
    }

    /// `FunctionRegistry`が返す(位置情報を持たない)`DbError::Eval`を、
    /// この式の位置を添えた`DbError::Bind`へ包み直す。
    fn wrap_eval_error(&self, err: DbError, span: Span) -> DbError {
        match err {
            DbError::Eval(message) => self.error_at(span, message),
            other => other,
        }
    }

    fn check_unary_type(&self, op: UnaryOperator, operand: &BoundExpr, span: Span) -> DbResult<DataType> {
        let operand_type = operand.data_type();
        match op {
            UnaryOperator::Negate => {
                if let Some(data_type) = operand_type
                    && data_type != DataType::BigInt
                {
                    return Err(self.error_at(span, format!("単項-はBIGINTに対してのみ使えます: {data_type}が渡されました")));
                }
                Ok(DataType::BigInt)
            }
            UnaryOperator::Not => {
                if let Some(data_type) = operand_type
                    && data_type != DataType::Boolean
                {
                    return Err(self.error_at(
                        span,
                        format!("論理演算はBOOLEANまたはNULLに対してのみ使えます: {data_type}が渡されました"),
                    ));
                }
                Ok(DataType::Boolean)
            }
        }
    }

    fn check_binary_type(
        &self,
        op: BinaryOperator,
        lhs: &BoundExpr,
        rhs: &BoundExpr,
        span: Span,
    ) -> DbResult<DataType> {
        let l = lhs.data_type();
        let r = rhs.data_type();
        match op {
            BinaryOperator::Add | BinaryOperator::Subtract | BinaryOperator::Multiply | BinaryOperator::Divide => {
                let l_ok = l.is_none() || l == Some(DataType::BigInt);
                let r_ok = r.is_none() || r == Some(DataType::BigInt);
                if !l_ok || !r_ok {
                    return Err(self.error_at(
                        span,
                        format!("算術演算はBIGINT同士にのみ使えます: {}と{}", describe_type(l), describe_type(r)),
                    ));
                }
                Ok(DataType::BigInt)
            }
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq => {
                let ok = match (l, r) {
                    (None, _) | (_, None) => true,
                    (Some(a), Some(b)) => a == b,
                };
                if !ok {
                    return Err(self.error_at(
                        span,
                        format!("比較演算は同じ型同士にのみ使えます: {}と{}", describe_type(l), describe_type(r)),
                    ));
                }
                Ok(DataType::Boolean)
            }
            BinaryOperator::And | BinaryOperator::Or => {
                // `eval::eval_bound_expr`がlhsを先に評価してからrhsを評価するのに
                // 合わせ、こちらもlhsを先に検査する。
                if let Some(data_type) = l
                    && data_type != DataType::Boolean
                {
                    return Err(self.error_at(
                        span,
                        format!("論理演算はBOOLEANまたはNULLに対してのみ使えます: {data_type}が渡されました"),
                    ));
                }
                if let Some(data_type) = r
                    && data_type != DataType::Boolean
                {
                    return Err(self.error_at(
                        span,
                        format!("論理演算はBOOLEANまたはNULLに対してのみ使えます: {data_type}が渡されました"),
                    ));
                }
                Ok(DataType::Boolean)
            }
        }
    }

    /// 列参照を解決する。
    ///
    /// `qualifier`が無い場合、`tables`を先頭から順に見て、その列名を持つ
    /// テーブルを探す。見つかったテーブルが2つ以上あれば、`DbError::Bind`で
    /// 曖昧さを報告する。`qualifier`がある場合は、まず`tables`の中から
    /// Alias(または、Aliasが無いテーブルなら名前そのもの)が一致するテーブルを
    /// 探し、そのテーブルの中だけで列名を探す。
    ///
    /// この関数は`tables`の要素数を`0`・`1`・`2`以上のどれとも仮定せずに書いて
    /// ある。現在の`Parser`は`FROM`に1テーブルしか持てないため、曖昧列を検出する
    /// 分岐(2件以上マッチ)は`bind_select`経由では到達しない。第22章の`JOIN`で
    /// `tables`が2要素以上になったときに、そのまま有効になる。
    fn resolve_column(
        &self,
        qualifier: Option<&Ident>,
        name: &str,
        span: Span,
        tables: &[BoundTableRef],
    ) -> DbResult<BoundExpr> {
        if let Some(qualifier) = qualifier {
            let (table_ordinal, table) = tables
                .iter()
                .enumerate()
                .find(|(_, table)| table.qualifier() == qualifier.name)
                .ok_or_else(|| {
                    self.error_at(qualifier.span, format!("テーブルまたはAlias'{}'が見つかりません", qualifier.name))
                })?;
            let column_index = table
                .schema
                .index_of(name)
                .ok_or_else(|| self.error_at(span, format!("列'{name}'は'{}'に存在しません", qualifier.name)))?;
            let data_type = table.schema.columns()[column_index].data_type;
            return Ok(BoundExpr::ColumnRef {
                table_ordinal,
                column_index,
                name: name.to_string(),
                data_type,
                span,
            });
        }

        let matches: Vec<(usize, usize, DataType)> = tables
            .iter()
            .enumerate()
            .filter_map(|(table_ordinal, table)| {
                table
                    .schema
                    .index_of(name)
                    .map(|column_index| (table_ordinal, column_index, table.schema.columns()[column_index].data_type))
            })
            .collect();

        match matches.as_slice() {
            [] => Err(self.error_at(span, format!("列'{name}'が見つかりません"))),
            [(table_ordinal, column_index, data_type)] => Ok(BoundExpr::ColumnRef {
                table_ordinal: *table_ordinal,
                column_index: *column_index,
                name: name.to_string(),
                data_type: *data_type,
                span,
            }),
            _ => {
                let owners: Vec<&str> = tables
                    .iter()
                    .filter(|table| table.schema.index_of(name).is_some())
                    .map(|table| table.qualifier())
                    .collect();
                Err(self.error_at(
                    span,
                    format!("列'{name}'は複数のテーブルに存在するため曖昧です: {}", owners.join(", ")),
                ))
            }
        }
    }
}

/// エラーメッセージ用に`Option<DataType>`を表示する。`None`(型が定まらない、
/// `NULL`リテラルなど)は`NULL`と表示する。
fn describe_type(data_type: Option<DataType>) -> String {
    match data_type {
        Some(t) => t.to_string(),
        None => "NULL".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn bind(sql: &str, catalog: &Catalog) -> DbResult<BoundStatement> {
        let statement = parse_statement(sql).unwrap();
        let functions = FunctionRegistry::with_builtins();
        Binder::new(catalog, &functions, sql).bind(statement)
    }

    fn bind_err_position(sql: &str, catalog: &Catalog) -> (usize, usize) {
        match bind(sql, catalog) {
            Err(DbError::Bind { line, column, .. }) => (line, column),
            other => panic!("DbError::Bindを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn unknown_table_is_rejected_with_position() {
        let catalog = users_catalog();
        let (line, column) = bind_err_position("SELECT id FROM does_not_exist", &catalog);
        // `does_not_exist`は16文字目(1始まり)から始まる。
        assert_eq!((line, column), (1, 16));
    }

    #[test]
    fn unknown_column_is_rejected_with_position() {
        let catalog = users_catalog();
        let (line, column) = bind_err_position("SELECT nickname FROM users", &catalog);
        assert_eq!((line, column), (1, 8));
    }

    #[test]
    fn type_mismatch_is_rejected_with_position() {
        let catalog = users_catalog();
        let (line, column) = bind_err_position("SELECT id FROM users WHERE id = 'x'", &catalog);
        // `WHERE`の中身、`id = 'x'`は28文字目(1始まり)から始まる。
        assert_eq!((line, column), (1, 28));
    }

    #[test]
    fn select_star_expands_to_all_columns_in_schema_order() {
        let catalog = users_catalog();
        let bound = bind("SELECT * FROM users", &catalog).unwrap();
        let BoundStatement::Select(select) = bound else {
            panic!("Selectを期待した");
        };
        let names: Vec<&str> = select.projection.iter().map(|item| item.output_name.as_str()).collect();
        assert_eq!(names, vec!["id", "name"]);
        for item in &select.projection {
            assert!(matches!(item.expr, BoundExpr::ColumnRef { .. }));
        }
    }

    #[test]
    fn table_alias_allows_qualified_column_ref() {
        let catalog = users_catalog();
        let bound = bind("SELECT u.id FROM users AS u WHERE u.name = 'Alice'", &catalog).unwrap();
        let BoundStatement::Select(select) = bound else {
            panic!("Selectを期待した");
        };
        assert_eq!(select.tables[0].alias.as_deref(), Some("u"));
        match &select.projection[0].expr {
            BoundExpr::ColumnRef { table_ordinal, column_index, .. } => {
                assert_eq!(*table_ordinal, 0);
                assert_eq!(*column_index, 0);
            }
            other => panic!("ColumnRefを期待したが{other:?}が返った"),
        }
        assert!(select.predicate.is_some());
    }

    #[test]
    fn alias_hides_the_original_table_name() {
        // `AS`でAliasを与えた場合、テーブル名そのものでの修飾は使えなくなる
        // (標準SQLの規則。`resolve_column`の`qualifier()`がAliasを優先するため)。
        let catalog = users_catalog();
        let result = bind("SELECT users.id FROM users AS u", &catalog);
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    #[test]
    fn unqualified_column_ref_still_resolves_without_alias() {
        let catalog = users_catalog();
        let bound = bind("SELECT users.id FROM users", &catalog).unwrap();
        let BoundStatement::Select(select) = bound else {
            panic!("Selectを期待した");
        };
        assert!(matches!(select.projection[0].expr, BoundExpr::ColumnRef { .. }));
    }

    #[test]
    fn ambiguous_column_is_rejected_across_multiple_tables() {
        // 現在のParserはFROMに1テーブルしか持てないため、この曖昧さはSQL文
        // からは作れない。第22章のJOINを見据えて`resolve_column`を直接呼び、
        // 複数テーブルへの一般化が実際に効くことを確認する。
        let functions = FunctionRegistry::with_builtins();
        let sql = "id";
        let binder = Binder::new(&EmptyCatalog, &functions, sql);
        let schema = Schema::new(vec![Column::new("id", DataType::BigInt, false)]);
        let tables = vec![
            BoundTableRef { table_id: TableId(0), table_name: "a".to_string(), alias: None, schema: schema.clone() },
            BoundTableRef { table_id: TableId(1), table_name: "b".to_string(), alias: None, schema },
        ];
        let result = binder.resolve_column(None, "id", Span::new(0, 2), &tables);
        assert!(matches!(result, Err(DbError::Bind { .. })));
    }

    struct EmptyCatalog;
    impl CatalogLookup for EmptyCatalog {
        fn table(&self, _name: &str) -> Option<&TableInfo> {
            None
        }
    }

    #[test]
    fn unknown_function_is_rejected_with_position() {
        let catalog = users_catalog();
        let (line, column) = bind_err_position("SELECT no_such_fn(id) FROM users", &catalog);
        assert_eq!((line, column), (1, 8));
    }

    #[test]
    fn insert_resolves_explicit_column_list() {
        let catalog = users_catalog();
        let bound = bind("INSERT INTO users (name, id) VALUES ('Alice', 1)", &catalog).unwrap();
        let BoundStatement::Insert(insert) = bound else {
            panic!("Insertを期待した");
        };
        assert_eq!(insert.columns, Some(vec![1, 0]));
    }

    #[test]
    fn insert_rejects_duplicate_column_with_position() {
        let catalog = users_catalog();
        let (line, column) = bind_err_position("INSERT INTO users (id, id) VALUES (1, 2)", &catalog);
        assert_eq!((line, column), (1, 24));
    }

    #[test]
    fn update_resolves_assignment_target_and_where() {
        let catalog = users_catalog();
        let bound = bind("UPDATE users SET name = 'Bob' WHERE id = 1", &catalog).unwrap();
        let BoundStatement::Update(update) = bound else {
            panic!("Updateを期待した");
        };
        assert_eq!(update.assignments[0].column_index, 1);
        assert!(update.predicate.is_some());
    }

    #[test]
    fn delete_resolves_where_against_the_target_table() {
        let catalog = users_catalog();
        let bound = bind("DELETE FROM users WHERE id = 1", &catalog).unwrap();
        let BoundStatement::Delete(delete) = bound else {
            panic!("Deleteを期待した");
        };
        assert!(delete.predicate.is_some());
    }

    #[test]
    fn drop_table_rejects_unknown_table_with_position() {
        let catalog = users_catalog();
        let (line, column) = bind_err_position("DROP TABLE does_not_exist", &catalog);
        assert_eq!((line, column), (1, 12));
    }

    #[test]
    fn create_table_passes_through_unchanged() {
        let catalog = users_catalog();
        let bound = bind("CREATE TABLE t (a BIGINT)", &catalog).unwrap();
        assert!(matches!(bound, BoundStatement::CreateTable(_)));
    }
}
