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
    AggregateFunc, AnalyzeStatement, Assignment, BeginStatement, BinaryOperator, CheckpointStatement, CommitStatement,
    CreateIndexStatement, CreateTableStatement, DeleteStatement, DropIndexStatement, DropTableStatement, Expr,
    FromClause, Ident, InsertStatement, JoinKind, RollbackStatement, SelectItem, SelectStatement, Statement,
    UnaryOperator, UpdateStatement,
};
use crate::catalog::{Catalog, TableInfo};
use crate::error::{DbError, DbResult};
use crate::eval::{FunctionRegistry, eval_bound_expr};
use crate::ids::TableId;
use crate::lexer::{self, Span};
use crate::logical_plan;
use crate::storage::Storage;
use crate::types::{Column, DataType, Schema};

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

    /// 索引名がすでに登録されているかどうか(第24章、`CREATE INDEX`・
    /// `DROP INDEX`の名前解決が使う)。既定は常に`false`であり、これは
    /// `Catalog`(メモリバックエンド、`Database::memory`)がそもそも索引という
    /// 概念を持たないことに対応する。索引を持つ`Storage`だけがこれを
    /// override する。
    fn index_exists(&self, _name: &str) -> bool {
        false
    }
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

    fn index_exists(&self, name: &str) -> bool {
        Storage::index(self, name).is_some()
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
    /// `JOIN`(第22章)で`joins`フィールドが加わり`BoundSelect`が大きくなった
    /// ため、他の(小さい)variantとのサイズ差を抑えるために`Box`で間接化する
    /// (`ast::Statement::Select`が`Box<SelectStatement>`にしているのと同じ理由)。
    Select(Box<BoundSelect>),
    CreateTable(CreateTableStatement),
    DropTable(DropTableStatement),
    /// `CREATE INDEX` / `CREATE UNIQUE INDEX`(第24章)。`CreateTable`とは違い、
    /// テーブル名・列名は既存のカタログエントリを指すため、`Binder`が
    /// `table_id`・`column_index`まで解決する。
    CreateIndex(BoundCreateIndex),
    /// `DROP INDEX`(第24章)。`DropTable`と同じく、索引の存在をここで確認し、
    /// 実行(カタログからの削除)は名前ベースのままでよいためASTをそのまま返す。
    DropIndex(DropIndexStatement),
    Insert(BoundInsert),
    Update(BoundUpdate),
    Delete(BoundDelete),
    /// `EXPLAIN [ANALYZE]`。対象の文はParser(第19章)がすでに`SELECT`・
    /// `INSERT INTO`・`UPDATE`・`DELETE FROM`の4種類に絞っているため、この
    /// 束縛先も`CreateTable`・`DropTable`・入れ子の`Explain`にはならない
    /// (`Database::execute_explain`はその前提で網羅する)。`analyze`は
    /// `ExplainStatement::analyze`(第27章)をそのまま引き継ぐ。
    Explain { inner: Box<BoundStatement>, analyze: bool },
    /// `ANALYZE`(第27章)。`CreateTable`と同じく、ASTのバリアントをそのまま
    /// 持ち回す(統計収集は既存のカタログエントリを書き換えるだけの操作であり、
    /// 式の名前解決・型検査を必要としない)。テーブル名が指定されていれば、
    /// その存在だけを`DropTable`と同じ理由でここで確認する。
    Analyze(AnalyzeStatement),
    /// `BEGIN`(第30章)。カタログと突き合わせる名前を持たないため、ASTの
    /// バリアントをそのまま持ち回す(`Analyze`と同じ理由)。
    Begin(BeginStatement),
    /// `COMMIT`(第30章)。
    Commit(CommitStatement),
    /// `ROLLBACK`(第30章)。
    Rollback(RollbackStatement),
    /// `CHECKPOINT`(第34章)。`Begin`・`Commit`・`Rollback`と同じ理由で、
    /// ASTのバリアントをそのまま持ち回す。
    Checkpoint(CheckpointStatement),
}

/// 束縛済みの`CREATE INDEX`(第24章)。
///
/// `table_name`・`index_name`・`column_name`は表示用(エラーメッセージ・
/// `Storage::create_index`の引数)に文字列のまま持つ。`table_id`・
/// `column_index`は`Database::execute_create_index`が直接使わない
/// (`Storage::create_index`は名前で引き直す、`CREATE TABLE`と同じ設計)が、
/// `Binder`が名前解決に成功した証拠として残してある。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundCreateIndex {
    pub index_name: String,
    pub table_name: String,
    pub table_id: TableId,
    pub column_name: String,
    pub column_index: usize,
    pub unique: bool,
    pub span: Span,
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

/// `FROM`が持つ1個の`JOIN`(第22章)。`tables[i + 1]`を`tables[0..=i]`(それまでに
/// 登場した全テーブル)へ結合する条件を表す。`tables`の要素数は
/// `joins.len() + 1`(`FROM`が空でない限り)になる。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundJoinStep {
    pub kind: JoinKind,
    /// `ON`に続く結合条件。`WHERE`と同じ`bind_predicate`で束縛するため、
    /// 必ずBOOLEAN(または型未定のNULL)を返す式になっている。
    pub condition: BoundExpr,
}

/// 束縛済みの`SELECT`。
///
/// `tables`は`FROM`が無ければ0個、`JOIN`が無ければ1個、`JOIN`が`n`個連なれば
/// `n + 1`個になる。要素数を固定した型ではなく`Vec`にしているのは、第17章の
/// 時点でこの型を先取りしておいたためである。[`Binder::resolve_column`]の
/// 曖昧列検出も、この`Vec`の要素数に関係なく動く形で書いてある。
///
/// `tables`に2個以上の要素があるとき、`BoundExpr::ColumnRef`の`column_index`は
/// 個々のテーブルの`Schema`内のローカルな添字ではなく、`tables`を左から右へ
/// 連結した**結合後スキーマ**上のフラットな添字になる(`table_ordinal`が`0`の
/// テーブルの列は`0..tables[0].schema.len()`、`1`のテーブルの列はその続き、
/// という並び)。これは`LogicalPlan::build_select`が`JOIN`を左深い木
/// (left-deep tree)として組み立てたとき、各`Join`ノードが生成する行が
/// 「左部分木の出力列」+「右側テーブルの列」という並びの結合行になり、
/// この並びが`tables`のフラットな添字とちょうど一致するように選んだ規則である
/// (詳細は`logical_plan`モジュール、`physical_plan`モジュールの解説を参照)。
///
/// `aggregate`が`Some`の場合、`projection`と`having`は`FROM`の列を直接指す
/// `ColumnRef`をもう含まない。集約が絡む`SELECT`では、`GROUP BY`の列と集約
/// 関数の呼び出しだけが下流(`HAVING`・射影・`ORDER BY`)から参照できる値の
/// 全てであり、`Binder::bind_select`はこの2種類を[`BoundAggregate::schema`]の
/// 列として並べ直したうえで、`projection`・`having`の式木に含まれる該当箇所を
/// その列への`ColumnRef`(`table_ordinal = 0`)へ書き換える(`Binder::rewrite_for_aggregate`
/// 参照)。`order_by`も同様に、常に`projection`が生成する出力列を指す
/// (「`ORDER BY`はどの範囲を束縛するか」節を参照)。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundSelect {
    pub tables: Vec<BoundTableRef>,
    /// `tables[i + 1]`を結合する`JOIN`の並び(第22章)。要素数は
    /// `tables.len().saturating_sub(1)`。
    pub joins: Vec<BoundJoinStep>,
    /// `SELECT DISTINCT`が指定されていたかどうか(第21章)。
    pub distinct: bool,
    /// `SELECT`が生成する行の列。末尾の`hidden_column_count`個は、`SELECT`が
    /// 宣言した出力には含まれない**隠し列**である(第21章、`Binder::bind_select`
    /// の「`ORDER BY`はどの範囲を束縛するか」を参照)。
    pub projection: Vec<BoundSelectItem>,
    /// `projection`のうち、末尾から数えて隠し列である個数。`0`なら`projection`
    /// はすべて`SELECT`の対象式であり、`LogicalPlan::build_select`は末尾を
    /// 切り落とす`Projection`を積まない。
    pub hidden_column_count: usize,
    pub predicate: Option<BoundExpr>,
    /// `GROUP BY`または集約関数の呼び出しを含む`SELECT`であれば`Some`
    /// (第21章)。`GROUP BY`が無くても`SELECT COUNT(*) FROM t`のように
    /// 集約関数だけを使う`SELECT`はここが`Some`になる(空の`Vec`を持つ
    /// `group_by`で、テーブル全体を1個のグループとして扱う)。`ORDER BY`だけに
    /// 集約関数呼び出しが現れる場合(`GROUP BY`も`HAVING`も無い`SELECT`に対する
    /// `ORDER BY COUNT(*)`)も、この`SELECT`全体を集約クエリとして扱う。
    pub aggregate: Option<BoundAggregate>,
    /// `HAVING`(第21章)。`aggregate`が`Some`の場合のみ`Some`になりうる
    /// (`Binder::bind_select`が、`HAVING`の存在自体を集約`SELECT`である
    /// ことの条件に含めているため)。
    pub having: Option<BoundExpr>,
    /// `ORDER BY`(第21章)。各要素は`projection`(隠し列を含む)の列を指す
    /// `ColumnRef`として束縛される。
    pub order_by: Vec<BoundOrderByItem>,
    /// `LIMIT`(第21章)。行を伴わない定数式として束縛の時点で評価し切った値
    /// (`Binder::eval_row_count_expr`)を持つ。
    pub limit: Option<usize>,
    /// `OFFSET`(第21章)。`limit`と同じ理由で束縛の時点で評価済み。
    pub offset: Option<usize>,
    pub span: Span,
}

/// 束縛済みの`ORDER BY`要素1個(第21章)。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundOrderByItem {
    pub expr: BoundExpr,
    pub desc: bool,
}

/// 束縛済みの集約情報(第21章)。
///
/// `group_by`はグループ化キーを計算するための式で、`FROM`の列を直接参照する
/// (集約前の行に対して評価する)。`calls`は`SELECT`・`HAVING`のどこかに現れた
/// 集約関数呼び出しを、最初に現れた順に重複無く集めたもの。`schema`は
/// `group_by`の列(先頭から`group_by.len()`列)に`calls`の列(残り)を続けた、
/// この演算子が生成する行の列構成である。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundAggregate {
    pub group_by: Vec<BoundExpr>,
    pub calls: Vec<AggregateCall>,
    pub schema: Schema,
}

/// 集約関数の呼び出し1個(第21章)。`arg`は`FROM`の列を直接参照する式で、
/// `None`は`COUNT(*)`だけを表す。
#[derive(Debug, Clone, PartialEq)]
pub struct AggregateCall {
    pub func: AggregateFunc,
    pub arg: Option<Box<BoundExpr>>,
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
    /// 列参照。`table_ordinal`は所属する[`BoundSelect::tables`]の添字。
    /// `column_index`は、テーブルが1個(`JOIN`が無い)の場合はそのテーブルの
    /// `Schema`上の列の添字と一致するが、`JOIN`で複数テーブルになった場合は
    /// **結合後スキーマ**(`tables`を左から右へ連結した列の並び)上のフラットな
    /// 添字になる(`BoundSelect`のドキュメント、「結合後スキーマ」の説明を参照)。
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
    /// 集約関数呼び出し(第21章)。`arg`が`None`なのは`COUNT(*)`だけ。
    ///
    /// `Binder::bind_select`は、集約が絡む`SELECT`ではこのノードを
    /// `BoundAggregate`の1列への`ColumnRef`へ書き換える(`rewrite_for_aggregate`)。
    /// そのため、`physical_plan`の`Executor`(`FilterExec`・`ProjectionExec`等)が
    /// 実際に評価する式木に、この`Aggregate`ノードが残ることは無い。残るのは
    /// `AggregateCall`(集約演算子自身がグループごとに計算する)としてだけである。
    Aggregate {
        func: AggregateFunc,
        arg: Option<Box<BoundExpr>>,
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
            | BoundExpr::Aggregate { data_type, .. }
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
            | BoundExpr::Aggregate { span, .. }
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
    pub table_name: String,
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
    pub table_name: String,
    pub schema: Schema,
    pub assignments: Vec<BoundAssignment>,
    pub predicate: Option<BoundExpr>,
    pub span: Span,
}

/// 束縛済みの`DELETE FROM`。
#[derive(Debug, Clone, PartialEq)]
pub struct BoundDelete {
    pub table_id: TableId,
    pub table_name: String,
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
            Statement::Select(select) => self.bind_select(*select).map(|select| BoundStatement::Select(Box::new(select))),
            Statement::CreateTable(create) => Ok(BoundStatement::CreateTable(create)),
            Statement::DropTable(drop) => self.bind_drop_table(drop),
            Statement::CreateIndex(create) => self.bind_create_index(create),
            Statement::DropIndex(drop) => self.bind_drop_index(drop),
            Statement::Insert(insert) => self.bind_insert(insert).map(BoundStatement::Insert),
            Statement::Update(update) => self.bind_update(update).map(BoundStatement::Update),
            Statement::Delete(delete) => self.bind_delete(delete).map(BoundStatement::Delete),
            Statement::Explain(explain) => {
                let analyze = explain.analyze;
                self.bind(*explain.statement).map(|inner| BoundStatement::Explain { inner: Box::new(inner), analyze })
            }
            Statement::Analyze(analyze) => self.bind_analyze(analyze),
            Statement::Begin(begin) => Ok(BoundStatement::Begin(begin)),
            Statement::Commit(commit) => Ok(BoundStatement::Commit(commit)),
            Statement::Rollback(rollback) => Ok(BoundStatement::Rollback(rollback)),
            Statement::Checkpoint(checkpoint) => Ok(BoundStatement::Checkpoint(checkpoint)),
        }
    }

    /// `ANALYZE`のテーブル名を解決する(第27章)。テーブル名が省略されて
    /// いれば(`table`が`None`)検査せずそのまま通す(`Database::execute_analyze`が
    /// 登録済みの全テーブルを対象にする)。
    fn bind_analyze(&self, analyze: AnalyzeStatement) -> DbResult<BoundStatement> {
        if let Some(table) = &analyze.table {
            self.catalog
                .table(&table.name)
                .ok_or_else(|| self.error_at(table.span, format!("テーブルが見つかりません: {}", table.name)))?;
        }
        Ok(BoundStatement::Analyze(analyze))
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

    /// `CREATE INDEX`のテーブル名・列名を解決する(第24章)。
    ///
    /// テーブルが存在しない、または`table`にその名前の列が無ければ位置情報つきの
    /// `DbError::Bind`にする。索引名がすでに使われているかどうかもここで検査する
    /// (`Storage::create_index`自身も`DbError::DuplicateIndex`として検出するが、
    /// `bind_drop_table`が採る方針と同じく、ここでは位置情報を持たせるために
    /// 先に検査する)。実行(`Database::execute_create_index`)は名前で
    /// `Storage::create_index`を呼び直す(`CREATE TABLE`と同じ設計)。
    fn bind_create_index(&self, create: CreateIndexStatement) -> DbResult<BoundStatement> {
        if self.catalog.index_exists(&create.index.name) {
            return Err(self.error_at(create.index.span, format!("索引はすでに存在します: {}", create.index.name)));
        }
        let info = self
            .catalog
            .table(&create.table.name)
            .ok_or_else(|| self.error_at(create.table.span, format!("テーブルが見つかりません: {}", create.table.name)))?;
        let column_index = info.schema.index_of(&create.column.name).ok_or_else(|| {
            self.error_at(create.column.span, format!("列が見つかりません: {}", create.column.name))
        })?;

        Ok(BoundStatement::CreateIndex(BoundCreateIndex {
            index_name: create.index.name,
            table_name: info.name.clone(),
            table_id: info.id,
            column_name: create.column.name,
            column_index,
            unique: create.unique,
            span: create.span,
        }))
    }

    /// `DROP INDEX`の索引名を解決する(第24章)。`bind_drop_table`と同じ理由で、
    /// 存在確認だけをここで行い、実行は名前ベースのまま`DropIndexStatement`を
    /// 素通りさせる。
    fn bind_drop_index(&self, drop: DropIndexStatement) -> DbResult<BoundStatement> {
        if !self.catalog.index_exists(&drop.index.name) {
            return Err(self.error_at(drop.index.span, format!("索引が見つかりません: {}", drop.index.name)));
        }
        Ok(BoundStatement::DropIndex(drop))
    }

    fn bind_select(&self, select: SelectStatement) -> DbResult<BoundSelect> {
        let (tables, joins) = self.bind_from(select.from.as_ref())?;

        let mut projection = Vec::with_capacity(select.items.len());
        for item in select.items {
            match item {
                SelectItem::Wildcard { span } => {
                    if tables.is_empty() {
                        return Err(self.error_at(span, "*はFROMを伴うSELECTでのみ使えます"));
                    }
                    // 複数テーブルの`*`は、テーブルの登場順→各テーブル内は列の
                    // 宣言順に展開する(`FROM a JOIN b ON ...`なら`a`の列、
                    // 続けて`b`の列)。`column_index`は個々のテーブル内の
                    // ローカルな添字ではなく、結合後スキーマ上のフラットな
                    // 添字にする(`BoundSelect`のドキュメント参照)。
                    let mut offset = 0;
                    for (table_ordinal, table) in tables.iter().enumerate() {
                        for (local_index, column) in table.schema.columns().iter().enumerate() {
                            projection.push(BoundSelectItem {
                                expr: BoundExpr::ColumnRef {
                                    table_ordinal,
                                    column_index: offset + local_index,
                                    name: column.name.clone(),
                                    data_type: column.data_type,
                                    span,
                                },
                                output_name: column.name.clone(),
                            });
                        }
                        offset += table.schema.len();
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
            Some(expr) => {
                let bound = self.bind_predicate(expr, &tables, "WHERE")?;
                if bound_contains_aggregate(&bound) {
                    return Err(self.error_at(
                        bound.span(),
                        "集約関数はWHEREでは使えません(集約はWHEREによる絞り込みの後に計算されます)".to_string(),
                    ));
                }
                Some(bound)
            }
            None => None,
        };

        let group_by = select
            .group_by
            .iter()
            .map(|expr| {
                let bound = self.bind_expr(expr, &tables)?;
                if bound_contains_aggregate(&bound) {
                    return Err(self.error_at(bound.span(), "GROUP BYの中では集約関数は使えません".to_string()));
                }
                Ok(bound)
            })
            .collect::<DbResult<Vec<_>>>()?;

        let raw_having = match &select.having {
            Some(expr) => Some(self.bind_predicate(expr, &tables, "HAVING")?),
            None => None,
        };

        // `ORDER BY`が新しく集約関数を持ち込む場合(`GROUP BY`も`HAVING`も無い
        // `SELECT`に対する`ORDER BY COUNT(*)`のような書き方)も、この`SELECT`を
        // 集約クエリとして扱う。構文木(`ast::Expr`)の時点で判定できるため、
        // まだ束縛していない`select.order_by`をそのまま調べる。
        let is_aggregate = !group_by.is_empty()
            || raw_having.is_some()
            || projection.iter().any(|item| bound_contains_aggregate(&item.expr))
            || select.order_by.iter().any(|item| ast_expr_contains_aggregate(&item.expr));

        let (mut aggregate, mut projection, having) = if is_aggregate {
            let mut aggregate = self.build_aggregate(group_by);
            let projection = projection
                .into_iter()
                .map(|item| {
                    let expr = self.rewrite_for_aggregate(item.expr, &mut aggregate)?;
                    Ok(BoundSelectItem { expr, output_name: item.output_name })
                })
                .collect::<DbResult<Vec<_>>>()?;
            let having = match raw_having {
                Some(expr) => Some(self.rewrite_for_aggregate(expr, &mut aggregate)?),
                None => None,
            };
            (Some(aggregate), projection, having)
        } else {
            (None, projection, None)
        };

        // `ORDER BY`の各式は、まず`SELECT`の対象式(`projection`)の中に同じ式が
        // 無いかを探し、あればその列をそのまま並べ替えのキーに使う(「射影に
        // 同名の出力列があればそれが優先される」という名前解決の優先順位)。
        // 見つからなければ、新しい列として`projection`の末尾に**隠し列**として
        // 追加する。隠し列は`SELECT`が宣言した出力には含まれず、並べ替えの
        // ためだけに`Sort`まで運ばれたあと、`LogicalPlan::build_select`が積む
        // 最後の`Projection`(トリム)で取り除かれる(詳細はモジュール冒頭の
        // 「`ORDER BY`はどの範囲を束縛するか」を参照)。
        let visible_len = projection.len();
        let mut order_by = Vec::with_capacity(select.order_by.len());
        for item in &select.order_by {
            let index = match aggregate.as_mut() {
                Some(aggregate) => {
                    self.resolve_order_by_in_aggregate_scope(&item.expr, &tables, aggregate, &mut projection)?
                }
                None => self.resolve_order_by_in_plain_scope(&item.expr, &tables, &mut projection)?,
            };
            if select.distinct && index >= visible_len {
                return Err(self.error_at(
                    item.expr.span(),
                    "DISTINCTを伴うSELECTでは、ORDER BYはSELECTの対象式だけを参照できます".to_string(),
                ));
            }
            let target = &projection[index];
            order_by.push(BoundOrderByItem {
                expr: BoundExpr::ColumnRef {
                    table_ordinal: 0,
                    column_index: index,
                    name: target.output_name.clone(),
                    data_type: target.expr.data_type().unwrap_or(DataType::Text),
                    span: item.expr.span(),
                },
                desc: item.desc,
            });
        }
        let hidden_column_count = projection.len() - visible_len;

        let limit = match &select.limit {
            Some(expr) => Some(self.eval_row_count_expr(expr, "LIMIT")?),
            None => None,
        };
        let offset = match &select.offset {
            Some(expr) => Some(self.eval_row_count_expr(expr, "OFFSET")?),
            None => None,
        };

        Ok(BoundSelect {
            tables,
            joins,
            distinct: select.distinct,
            projection,
            predicate,
            aggregate,
            having,
            order_by,
            hidden_column_count,
            limit,
            offset,
            span: select.span,
        })
    }

    /// `group_by`から、集約結果の列構成の先頭部分(グループ化キー)だけを
    /// 確定させた[`BoundAggregate`]を作る。集約関数の呼び出し(`calls`)は
    /// まだ1つも登録されていない状態で始まり、[`Binder::rewrite_for_aggregate`]
    /// が`projection`・`having`・`ORDER BY`を書き換える過程で、新しい呼び出しに
    /// 出会うたびに追記されていく。
    fn build_aggregate(&self, group_by: Vec<BoundExpr>) -> BoundAggregate {
        let columns = group_by
            .iter()
            .map(|expr| {
                let data_type = expr.data_type().unwrap_or(DataType::Text);
                Column::new(logical_plan::fmt_bound_expr(expr), data_type, true)
            })
            .collect();
        BoundAggregate { group_by, calls: Vec::new(), schema: Schema::new(columns) }
    }

    /// `expr`の中で、`aggregate.group_by`の式全体と一致する部分式、または
    /// 集約関数呼び出し(`BoundExpr::Aggregate`)を、`aggregate.schema`の対応する
    /// 列への`ColumnRef`へ置き換える。
    ///
    /// 一致するかどうかは式の構造を表す文字列表現(`logical_plan::fmt_bound_expr`、
    /// `Span`を含まないため書かれた位置に関係なく同じ式なら同じ文字列になる)を
    /// 比較して判定し、一致すればそれ以上式の内部には再帰しない。`GROUP BY
    /// a + b`のもとで`SELECT a + b`と書いた場合、`a + b`全体が1つのグループ化
    /// キーとして一致するため、内部の`a`・`b`を個別に検査する必要が無い(むしろ
    /// `a`・`b`を個別に検査すると、集約後にはもう存在しない`FROM`の生の列参照
    /// として誤って拒否してしまう)。
    ///
    /// 一致しない`ColumnRef`は、`GROUP BY`にも集約関数の中にも現れない、
    /// 集約後の値が定まらない列参照であり、標準SQLの「関数従属性」の違反として
    /// エラーにする(`SELECT name FROM t GROUP BY dept`のような文が典型)。
    ///
    /// 集約関数呼び出しは、`aggregate.calls`の中にまだ同じ呼び出しが無ければ
    /// 新しい列として追記する。この関数は`projection`・`having`・`ORDER BY`の
    /// 書き換えに共通して使われるため、`SELECT dept, COUNT(*) FROM t GROUP BY
    /// dept ORDER BY COUNT(*) DESC`のように、`ORDER BY`だけに現れる集約呼び出し
    /// も、`projection`に現れる呼び出しと同じ扱いで`aggregate.schema`の列になる。
    fn rewrite_for_aggregate(&self, expr: BoundExpr, aggregate: &mut BoundAggregate) -> DbResult<BoundExpr> {
        let key = logical_plan::fmt_bound_expr(&expr);
        if let Some(slot) = aggregate.group_by.iter().position(|g| logical_plan::fmt_bound_expr(g) == key) {
            return Ok(self.slot_column_ref(slot, &aggregate.schema, expr.span()));
        }

        match expr {
            BoundExpr::Aggregate { func, arg, span, .. } => {
                let existing = aggregate.calls.iter().position(|call| logical_plan::fmt_aggregate_call(call) == key);
                let slot = match existing {
                    Some(index) => aggregate.group_by.len() + index,
                    None => {
                        let data_type = self.check_aggregate_arg_type(func, arg.as_deref(), span)?;
                        let label = format!(
                            "{}({})",
                            func.name(),
                            arg.as_deref().map(logical_plan::fmt_bound_expr).unwrap_or_else(|| "*".to_string())
                        );
                        let nullable = !matches!(func, AggregateFunc::Count);
                        aggregate.schema.columns_mut().push(Column::new(label, data_type, nullable));
                        aggregate.calls.push(AggregateCall { func, arg });
                        aggregate.group_by.len() + aggregate.calls.len() - 1
                    }
                };
                Ok(self.slot_column_ref(slot, &aggregate.schema, span))
            }
            BoundExpr::ColumnRef { span, name, .. } => Err(self.error_at(
                span,
                format!("列'{name}'はGROUP BYの列か集約関数の引数としてのみ使用できます"),
            )),
            BoundExpr::IntLiteral { .. }
            | BoundExpr::StringLiteral { .. }
            | BoundExpr::BoolLiteral { .. }
            | BoundExpr::NullLiteral { .. } => Ok(expr),
            BoundExpr::UnaryOp { op, expr, data_type, span } => {
                let expr = self.rewrite_for_aggregate(*expr, aggregate)?;
                Ok(BoundExpr::UnaryOp { op, expr: Box::new(expr), data_type, span })
            }
            BoundExpr::BinaryOp { op, lhs, rhs, data_type, span } => {
                let lhs = self.rewrite_for_aggregate(*lhs, aggregate)?;
                let rhs = self.rewrite_for_aggregate(*rhs, aggregate)?;
                Ok(BoundExpr::BinaryOp { op, lhs: Box::new(lhs), rhs: Box::new(rhs), data_type, span })
            }
            BoundExpr::IsNull { expr, negated, span } => {
                let expr = self.rewrite_for_aggregate(*expr, aggregate)?;
                Ok(BoundExpr::IsNull { expr: Box::new(expr), negated, span })
            }
            BoundExpr::FunctionCall { name, args, data_type, span } => {
                let args =
                    args.into_iter().map(|arg| self.rewrite_for_aggregate(arg, aggregate)).collect::<DbResult<Vec<_>>>()?;
                Ok(BoundExpr::FunctionCall { name, args, data_type, span })
            }
            BoundExpr::Paren { expr, span } => {
                let expr = self.rewrite_for_aggregate(*expr, aggregate)?;
                Ok(BoundExpr::Paren { expr: Box::new(expr), span })
            }
            BoundExpr::Cast { expr, data_type, span } => {
                let expr = self.rewrite_for_aggregate(*expr, aggregate)?;
                Ok(BoundExpr::Cast { expr: Box::new(expr), data_type, span })
            }
        }
    }

    fn slot_column_ref(&self, slot: usize, schema: &Schema, span: Span) -> BoundExpr {
        let column = &schema.columns()[slot];
        BoundExpr::ColumnRef {
            table_ordinal: 0,
            column_index: slot,
            name: column.name.clone(),
            data_type: column.data_type,
            span,
        }
    }

    /// `ORDER BY`の式を、集約を伴わない`SELECT`のスコープ(`tables`、`WHERE`や
    /// 元の`projection`と同じ)で束縛する。
    ///
    /// `projection`(`SELECT`の対象式)の中にすでに同じ式(`logical_plan::fmt_bound_expr`
    /// による構造の一致)があれば、新しい列を増やさずその列を指す添字を返す。
    /// 無ければ、`projection`の末尾に隠し列として追加してその添字を返す
    /// (`Binder::bind_select`のドキュメント参照)。
    fn resolve_order_by_in_plain_scope(
        &self,
        expr: &Expr,
        tables: &[BoundTableRef],
        projection: &mut Vec<BoundSelectItem>,
    ) -> DbResult<usize> {
        let bound = self.bind_expr(expr, tables)?;
        let key = logical_plan::fmt_bound_expr(&bound);
        if let Some(index) = projection.iter().position(|item| logical_plan::fmt_bound_expr(&item.expr) == key) {
            return Ok(index);
        }
        let output_name = self.sql[expr.span().start..expr.span().end].to_string();
        projection.push(BoundSelectItem { expr: bound, output_name });
        Ok(projection.len() - 1)
    }

    /// `resolve_order_by_in_plain_scope`の集約クエリ版。`expr`を`tables`(`HAVING`
    /// と同じスコープ、集約関数呼び出しを含んでよい)で束縛したうえで、
    /// `Binder::rewrite_for_aggregate`に通す。まだ`projection`にも`having`にも
    /// 現れていない集約関数呼び出し(`ORDER BY COUNT(*) DESC`のような)であれば、
    /// この呼び出しの中で`aggregate.schema`へ新しい列として追記される。
    fn resolve_order_by_in_aggregate_scope(
        &self,
        expr: &Expr,
        tables: &[BoundTableRef],
        aggregate: &mut BoundAggregate,
        projection: &mut Vec<BoundSelectItem>,
    ) -> DbResult<usize> {
        let bound = self.bind_expr(expr, tables)?;
        let rewritten = self.rewrite_for_aggregate(bound, aggregate)?;
        let key = logical_plan::fmt_bound_expr(&rewritten);
        if let Some(index) = projection.iter().position(|item| logical_plan::fmt_bound_expr(&item.expr) == key) {
            return Ok(index);
        }
        let output_name = self.sql[expr.span().start..expr.span().end].to_string();
        projection.push(BoundSelectItem { expr: rewritten, output_name });
        Ok(projection.len() - 1)
    }

    /// `LIMIT`・`OFFSET`の式を束縛し、その場で評価して非負の`usize`にする。
    ///
    /// `LIMIT`・`OFFSET`は特定の行を参照しない定数式であり(標準SQLも実行時に
    /// 行ごとに変わる値を許さない)、`Binder`はこれを`tables`を持たない空の
    /// スコープで束縛する。これにより`LIMIT id`のような列参照は、構文としては
    /// 書けても「列'id'が見つかりません」という束縛エラーになる。値は
    /// `eval_bound_expr`でこの場で評価してしまい、以後(`LogicalPlan`・
    /// `PhysicalPlan`)は評価済みの`usize`として持ち回る。行に依存しない値を
    /// 実行のたびに再評価する理由が無いためである。
    fn eval_row_count_expr(&self, expr: &Expr, clause: &str) -> DbResult<usize> {
        let bound = self.bind_expr(expr, &[])?;
        match bound.data_type() {
            Some(DataType::BigInt) => {}
            other => {
                return Err(self.error_at(
                    bound.span(),
                    format!("{clause}はBIGINTを返す式である必要があります: {}", describe_type(other)),
                ));
            }
        }
        let span = bound.span();
        let value = eval_bound_expr(&bound, self.functions, None).map_err(|err| self.wrap_eval_error(err, span))?;
        match value {
            crate::types::Value::BigInt(n) if n >= 0 => Ok(n as usize),
            crate::types::Value::BigInt(n) => Err(self.error_at(span, format!("{clause}に負の値は指定できません: {n}"))),
            crate::types::Value::Null => Err(self.error_at(span, format!("{clause}にNULLは指定できません"))),
            _ => unreachable!("data_type()の検査でBIGINT以外は既に弾いている"),
        }
    }

    /// `FROM`(あれば)を、単一テーブルの参照または`JOIN`の連鎖として束縛する。
    ///
    /// 返り値の1つ目はテーブルを左から右へ並べた`Vec`、2つ目はそれぞれの
    /// `JOIN`の`ON`条件を同じ順序で並べた`Vec`(要素数はテーブルの個数より
    /// 1つ少ない)である。`i`番目の`JOIN`の`ON`条件は`tables[0..=i+1]`
    /// (それまでに`FROM`へ登場した全テーブル)のスコープで束縛するため、
    /// `FROM a JOIN b ON a.x = b.x JOIN c ON a.y = c.y`のように、3番目以降の
    /// `JOIN`条件が直前のテーブルだけでなくそれより前のテーブルも参照できる
    /// (標準SQLが認める規則であり、`Binder`もこれに従う)。
    fn bind_from(&self, from: Option<&FromClause>) -> DbResult<(Vec<BoundTableRef>, Vec<BoundJoinStep>)> {
        let Some(from) = from else {
            return Ok((Vec::new(), Vec::new()));
        };

        let mut tables = vec![self.resolve_table(&from.table, from.alias.as_ref())?];
        let mut joins = Vec::with_capacity(from.joins.len());
        for join in &from.joins {
            let right = self.resolve_table(&join.table, join.alias.as_ref())?;
            tables.push(right);
            let condition = self.bind_predicate(&join.on, &tables, "ON")?;
            if bound_contains_aggregate(&condition) {
                return Err(self.error_at(
                    condition.span(),
                    "集約関数はON句では使えません(集約はJOINの後に計算されます)".to_string(),
                ));
            }
            joins.push(BoundJoinStep { kind: join.kind, condition });
        }
        Ok((tables, joins))
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
            table_name: table.table_name,
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
            Some(expr) => Some(self.bind_predicate(expr, tables, "WHERE")?),
            None => None,
        };

        Ok(BoundUpdate {
            table_id: table.table_id,
            table_name: table.table_name,
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
            Some(expr) => Some(self.bind_predicate(expr, tables, "WHERE")?),
            None => None,
        };

        Ok(BoundDelete {
            table_id: table.table_id,
            table_name: table.table_name,
            schema: table.schema,
            predicate,
            span: delete.span,
        })
    }

    /// `WHERE`句を束縛したうえで、`BOOLEAN`(または型未定の`NULL`)を返す式に
    /// なっていることを検査する。`executor::check_predicate_type`(第10章)が
    /// 行っていた検査と同じ規則を、束縛の時点でまとめて行う。
    /// `clause`(`"WHERE"`・`"HAVING"`・`"ON"`)は、型不一致のエラーメッセージに
    /// 使う句の名前。`WHERE`・`HAVING`・`JOIN`の`ON`は、どれも「行を絞り込む
    /// (または結合する)条件がBOOLEANを返す式でなければならない」という同じ
    /// 検査を共有しているが、エラーメッセージにはどの句が違反したのかを
    /// 正確に示す。
    fn bind_predicate(&self, expr: &Expr, tables: &[BoundTableRef], clause: &str) -> DbResult<BoundExpr> {
        let bound = self.bind_expr(expr, tables)?;
        match bound.data_type() {
            Some(DataType::Boolean) | None => Ok(bound),
            Some(other) => Err(self.error_at(
                bound.span(),
                format!("{clause}句はBOOLEANを返す式である必要があります: 式の型は{other}です"),
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
            Expr::Aggregate { func, arg, span } => self.bind_aggregate(*func, arg.as_deref(), *span, tables),
        }
    }

    /// 集約関数呼び出しを束縛する。`arg`(`COUNT(*)`なら`None`)は`tables`の
    /// 列を直接参照する式として`bind_expr`で束縛したうえで、その中にさらに
    /// 集約関数が現れていないか(`SUM(COUNT(x))`のような入れ子)を検査する。
    /// SQLは集約関数の入れ子を許さない。集約は「複数行を1行へ畳み込む」
    /// 演算であり、`COUNT(x)`の結果はすでに1つのグループにつき1個の値なので、
    /// それをさらに`SUM`で畳み込む対象(複数行)がその場に存在しないからである。
    fn bind_aggregate(
        &self,
        func: AggregateFunc,
        arg: Option<&Expr>,
        span: Span,
        tables: &[BoundTableRef],
    ) -> DbResult<BoundExpr> {
        let bound_arg = match arg {
            Some(expr) => {
                let bound = self.bind_expr(expr, tables)?;
                if bound_contains_aggregate(&bound) {
                    return Err(self.error_at(bound.span(), "集約関数は入れ子にできません".to_string()));
                }
                Some(Box::new(bound))
            }
            None => None,
        };
        let data_type = self.check_aggregate_arg_type(func, bound_arg.as_deref(), span)?;
        Ok(BoundExpr::Aggregate { func, arg: bound_arg, data_type, span })
    }

    /// 集約関数が引数に課す型制約を検査し、戻り値の`DataType`を決める。
    ///
    /// `COUNT`は引数の型を問わない(`COUNT(*)`は引数を持たず、`COUNT(x)`は
    /// `x`が`NULL`かどうかしか見ない)ので常に`BIGINT`を返す。`SUM`は
    /// このSQLサブセットが算術演算を`BIGINT`同士にしか許していない(第8章)のと
    /// 揃え、`BIGINT`の列にのみ使える。`MIN`・`MAX`は比較演算(第8章)が
    /// `BIGINT`・`TEXT`・`BOOLEAN`のどの型同士でも定義されているのに合わせ、
    /// 型を問わず引数の型をそのまま返す(引数が型を持たない`NULL`単体の場合は
    /// `projection_schema`の他の箇所と同じ規則で`TEXT`を代用する)。
    fn check_aggregate_arg_type(
        &self,
        func: AggregateFunc,
        arg: Option<&BoundExpr>,
        span: Span,
    ) -> DbResult<DataType> {
        match func {
            AggregateFunc::Count => Ok(DataType::BigInt),
            AggregateFunc::Sum => {
                let arg = arg.expect("ParserはSUMに必ず引数を1個持たせる");
                if let Some(data_type) = arg.data_type()
                    && data_type != DataType::BigInt
                {
                    return Err(self.error_at(span, format!("SUMはBIGINTに対してのみ使えます: {data_type}が渡されました")));
                }
                Ok(DataType::BigInt)
            }
            AggregateFunc::Min | AggregateFunc::Max => {
                let arg = arg.expect("ParserはMIN/MAXに必ず引数を1個持たせる");
                Ok(arg.data_type().unwrap_or(DataType::Text))
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
            let local_index = table
                .schema
                .index_of(name)
                .ok_or_else(|| self.error_at(span, format!("列'{name}'は'{}'に存在しません", qualifier.name)))?;
            let data_type = table.schema.columns()[local_index].data_type;
            let column_index = table_offset(tables, table_ordinal) + local_index;
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
                table.schema.index_of(name).map(|local_index| {
                    let column_index = table_offset(tables, table_ordinal) + local_index;
                    (table_ordinal, column_index, table.schema.columns()[local_index].data_type)
                })
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

/// `tables[..table_ordinal]`の列数の合計。結合後スキーマ上での、
/// `table_ordinal`番目のテーブルの先頭列が占める添字を返す
/// (`BoundSelect`のドキュメント、「結合後スキーマ」の説明を参照)。
/// `table_ordinal`が`0`のときは常に`0`を返すため、`JOIN`を持たない
/// (`tables`の要素数が1個の)`SELECT`では、このオフセットは常に`0`のままで
/// `column_index`はこれまでどおりテーブル内のローカルな添字と一致する。
fn table_offset(tables: &[BoundTableRef], table_ordinal: usize) -> usize {
    tables[..table_ordinal].iter().map(|table| table.schema.len()).sum()
}

/// エラーメッセージ用に`Option<DataType>`を表示する。`None`(型が定まらない、
/// `NULL`リテラルなど)は`NULL`と表示する。
fn describe_type(data_type: Option<DataType>) -> String {
    match data_type {
        Some(t) => t.to_string(),
        None => "NULL".to_string(),
    }
}

/// `expr`の式木のどこかに`BoundExpr::Aggregate`が現れるかどうか。
///
/// `WHERE`・`GROUP BY`に集約関数が使えないことの検査(`Binder::bind_select`)と、
/// 集約関数の引数の中にさらに集約関数が現れていないこと(入れ子の禁止、
/// `Binder::bind_aggregate`)の両方で使う共通の判定である。
fn bound_contains_aggregate(expr: &BoundExpr) -> bool {
    match expr {
        BoundExpr::Aggregate { .. } => true,
        BoundExpr::IntLiteral { .. }
        | BoundExpr::StringLiteral { .. }
        | BoundExpr::BoolLiteral { .. }
        | BoundExpr::NullLiteral { .. }
        | BoundExpr::ColumnRef { .. } => false,
        BoundExpr::UnaryOp { expr, .. } => bound_contains_aggregate(expr),
        BoundExpr::BinaryOp { lhs, rhs, .. } => bound_contains_aggregate(lhs) || bound_contains_aggregate(rhs),
        BoundExpr::IsNull { expr, .. } => bound_contains_aggregate(expr),
        BoundExpr::FunctionCall { args, .. } => args.iter().any(bound_contains_aggregate),
        BoundExpr::Paren { expr, .. } => bound_contains_aggregate(expr),
        BoundExpr::Cast { expr, .. } => bound_contains_aggregate(expr),
    }
}

/// `expr`(構文解析直後の`ast::Expr`、まだ束縛していない)の式木のどこかに
/// `Expr::Aggregate`が現れるかどうか。
///
/// `Binder::bind_select`が「この`SELECT`を集約クエリとして扱うか」を判定する
/// 材料の1つとして使う。`ORDER BY`はまだ束縛していない(束縛は`projection`が
/// 確定してから行う、モジュール冒頭の説明を参照)ため、`bound_contains_aggregate`
/// (`BoundExpr`版)ではなくこちらのAST版を使う。
fn ast_expr_contains_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Aggregate { .. } => true,
        Expr::IntLiteral { .. }
        | Expr::StringLiteral { .. }
        | Expr::BoolLiteral { .. }
        | Expr::NullLiteral { .. }
        | Expr::ColumnRef { .. } => false,
        Expr::UnaryOp { expr, .. } => ast_expr_contains_aggregate(expr),
        Expr::BinaryOp { lhs, rhs, .. } => ast_expr_contains_aggregate(lhs) || ast_expr_contains_aggregate(rhs),
        Expr::IsNull { expr, .. } => ast_expr_contains_aggregate(expr),
        Expr::FunctionCall { args, .. } => args.iter().any(ast_expr_contains_aggregate),
        Expr::Paren { expr, .. } => ast_expr_contains_aggregate(expr),
        Expr::Cast { expr, .. } => ast_expr_contains_aggregate(expr),
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

    /// `JOIN`のテスト用に、`a(id, x)`・`b(id, y)`という、あえて同名の`id`列を
    /// 両方に持つ2テーブルのカタログ。
    fn ab_catalog() -> Catalog {
        let mut catalog = Catalog::new();
        catalog
            .create_table("a", Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("x", DataType::Text, true)]))
            .unwrap();
        catalog
            .create_table("b", Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("y", DataType::Text, true)]))
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
        // `resolve_column`を直接呼ぶ形の単体テスト。`tables`の要素数に
        // 関係なく曖昧列検出が働くことを確認する(下の`ambiguous_column_
        // via_join_is_rejected`が、実際のJOIN構文から同じ分岐に到達することを
        // 確認する)。
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

    // ---- JOIN(第22章) ----

    #[test]
    fn ambiguous_column_via_join_is_rejected() {
        // `a`・`b`はどちらも`id`という列を持つ。`JOIN`で`tables`が2要素になった
        // ことで、`ambiguous_column_is_rejected_across_multiple_tables`が
        // 単体テストとしてのみ確認していた分岐に、実際のSQL文から到達できる。
        let catalog = ab_catalog();
        let err = bind("SELECT id FROM a JOIN b ON a.id = b.id", &catalog).unwrap_err();
        match err {
            DbError::Bind { message, .. } => assert!(message.contains("曖昧です")),
            other => panic!("DbError::Bindを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn qualified_column_ref_resolves_across_joined_tables() {
        let catalog = ab_catalog();
        let bound = bind("SELECT a.id, b.y FROM a JOIN b ON a.id = b.id", &catalog).unwrap();
        let BoundStatement::Select(select) = bound else {
            panic!("Selectを期待した");
        };
        assert_eq!(select.tables.len(), 2);
        // `a`は2列(id, x)なので、`b`の列の結合後スキーマ上の添字は2から始まる。
        // `b.y`は`b`の中ではローカル添字1(0番目がid)なので、結合後は2+1=3。
        match &select.projection[1].expr {
            BoundExpr::ColumnRef { table_ordinal, column_index, .. } => {
                assert_eq!(*table_ordinal, 1);
                assert_eq!(*column_index, 3);
            }
            other => panic!("ColumnRefを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn select_star_after_join_expands_left_table_then_right_table() {
        let catalog = ab_catalog();
        let bound = bind("SELECT * FROM a JOIN b ON a.id = b.id", &catalog).unwrap();
        let BoundStatement::Select(select) = bound else {
            panic!("Selectを期待した");
        };
        let names: Vec<&str> = select.projection.iter().map(|item| item.output_name.as_str()).collect();
        assert_eq!(names, vec!["id", "x", "id", "y"]);
    }

    #[test]
    fn chained_join_condition_can_reference_an_earlier_table() {
        // `c`の結合条件が、直前の`b`だけでなく最初の`a`も参照できることを確認する。
        let mut catalog = ab_catalog();
        catalog
            .create_table("c", Schema::new(vec![Column::new("id", DataType::BigInt, false), Column::new("z", DataType::Text, true)]))
            .unwrap();
        let bound =
            bind("SELECT a.id FROM a JOIN b ON a.id = b.id JOIN c ON a.id = c.id", &catalog).unwrap();
        let BoundStatement::Select(select) = bound else {
            panic!("Selectを期待した");
        };
        assert_eq!(select.tables.len(), 3);
        assert_eq!(select.joins.len(), 2);
    }

    #[test]
    fn on_clause_must_be_boolean() {
        let catalog = ab_catalog();
        let err = bind("SELECT a.id FROM a JOIN b ON a.id", &catalog).unwrap_err();
        match err {
            DbError::Bind { message, .. } => assert!(message.contains("BOOLEAN")),
            other => panic!("DbError::Bindを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn aggregate_in_on_clause_is_rejected() {
        let catalog = ab_catalog();
        let err = bind("SELECT a.id FROM a JOIN b ON a.id = COUNT(*)", &catalog).unwrap_err();
        match err {
            DbError::Bind { message, .. } => assert!(message.contains("ON句")),
            other => panic!("DbError::Bindを期待したが{other:?}が返った"),
        }
    }

    #[test]
    fn unknown_table_in_join_is_rejected() {
        let catalog = ab_catalog();
        let err = bind("SELECT a.id FROM a JOIN does_not_exist ON a.id = 1", &catalog).unwrap_err();
        assert!(matches!(err, DbError::Bind { .. }));
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
    fn analyze_with_a_known_table_passes_through_unchanged() {
        let catalog = users_catalog();
        let bound = bind("ANALYZE users", &catalog).unwrap();
        let BoundStatement::Analyze(analyze) = bound else {
            panic!("Analyzeを期待した");
        };
        assert_eq!(analyze.table.map(|t| t.name), Some("users".to_string()));
    }

    #[test]
    fn analyze_without_a_table_name_passes_through_unchanged() {
        let catalog = users_catalog();
        let bound = bind("ANALYZE", &catalog).unwrap();
        assert!(matches!(bound, BoundStatement::Analyze(_)));
    }

    #[test]
    fn analyze_rejects_unknown_table_with_position() {
        let catalog = users_catalog();
        let (line, column) = bind_err_position("ANALYZE does_not_exist", &catalog);
        assert_eq!((line, column), (1, 9));
    }

    #[test]
    fn explain_analyze_keeps_the_analyze_flag_through_binding() {
        let catalog = users_catalog();
        let bound = bind("EXPLAIN ANALYZE SELECT id FROM users", &catalog).unwrap();
        let BoundStatement::Explain { analyze, .. } = bound else {
            panic!("Explainを期待した");
        };
        assert!(analyze);
    }

    #[test]
    fn explain_without_analyze_keeps_the_flag_false_through_binding() {
        let catalog = users_catalog();
        let bound = bind("EXPLAIN SELECT id FROM users", &catalog).unwrap();
        let BoundStatement::Explain { analyze, .. } = bound else {
            panic!("Explainを期待した");
        };
        assert!(!analyze);
    }

    #[test]
    fn create_table_passes_through_unchanged() {
        let catalog = users_catalog();
        let bound = bind("CREATE TABLE t (a BIGINT)", &catalog).unwrap();
        assert!(matches!(bound, BoundStatement::CreateTable(_)));
    }

    // ---- 第21章: ORDER BY / LIMIT / OFFSET / DISTINCT / GROUP BY / HAVING / 集約 ----

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

    fn bind_select_orders(sql: &str) -> BoundSelect {
        let catalog = orders_catalog();
        match bind(sql, &catalog).unwrap() {
            BoundStatement::Select(select) => *select,
            other => panic!("Selectを期待したが{other:?}が返った"),
        }
    }

    fn bind_err_orders(sql: &str) -> DbError {
        let catalog = orders_catalog();
        bind(sql, &catalog).unwrap_err()
    }

    #[test]
    fn select_without_group_by_or_aggregate_has_no_aggregate_info() {
        let select = bind_select_orders("SELECT dept, amount FROM orders");
        assert!(select.aggregate.is_none());
    }

    #[test]
    fn count_star_alone_is_an_aggregate_query_without_group_by() {
        let select = bind_select_orders("SELECT COUNT(*) FROM orders");
        let aggregate = select.aggregate.unwrap();
        assert!(aggregate.group_by.is_empty());
        assert_eq!(aggregate.calls.len(), 1);
        assert_eq!(aggregate.schema.columns().len(), 1);
    }

    #[test]
    fn group_by_and_aggregate_calls_become_output_schema_columns() {
        let select = bind_select_orders("SELECT dept, COUNT(*), SUM(amount) FROM orders GROUP BY dept");
        let aggregate = select.aggregate.unwrap();
        assert_eq!(aggregate.group_by.len(), 1);
        assert_eq!(aggregate.calls.len(), 2);
        let names: Vec<&str> = aggregate.schema.columns().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["dept", "COUNT(*)", "SUM(amount)"]);
    }

    #[test]
    fn duplicate_aggregate_calls_share_one_output_slot() {
        // `COUNT(*)`が`SELECT`に2回現れても、`AggregateExec`が二重に計算しない
        // よう出力列は1列にまとめる。
        let select = bind_select_orders("SELECT COUNT(*), COUNT(*) FROM orders");
        let aggregate = select.aggregate.unwrap();
        assert_eq!(aggregate.calls.len(), 1);
        assert_eq!(aggregate.schema.columns().len(), 1);
        // 2つの射影項目はどちらも同じ列(添字0)を指す。
        for item in &select.projection {
            match &item.expr {
                BoundExpr::ColumnRef { column_index, .. } => assert_eq!(*column_index, 0),
                other => panic!("書き換え後はColumnRefになるはずが{other:?}"),
            }
        }
    }

    #[test]
    fn projecting_a_non_grouped_column_is_a_functional_dependency_violation() {
        let err = bind_err_orders("SELECT dept, amount FROM orders GROUP BY dept");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn having_can_reference_an_aggregate_not_in_the_select_list() {
        let select = bind_select_orders("SELECT dept FROM orders GROUP BY dept HAVING COUNT(*) > 1");
        let aggregate = select.aggregate.unwrap();
        // `HAVING`だけに現れた`COUNT(*)`も、`dept`に続く出力列として確保される。
        assert_eq!(aggregate.calls.len(), 1);
        assert!(select.having.is_some());
    }

    #[test]
    fn having_referencing_a_non_grouped_column_is_rejected() {
        let err = bind_err_orders("SELECT dept FROM orders GROUP BY dept HAVING amount > 1");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn nested_aggregate_is_rejected() {
        let err = bind_err_orders("SELECT SUM(COUNT(*)) FROM orders");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn aggregate_in_where_is_rejected() {
        let err = bind_err_orders("SELECT dept FROM orders WHERE COUNT(*) > 1");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn aggregate_in_group_by_is_rejected() {
        let err = bind_err_orders("SELECT dept FROM orders GROUP BY COUNT(*)");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn sum_requires_a_bigint_argument() {
        let err = bind_err_orders("SELECT SUM(dept) FROM orders");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn min_max_accept_any_comparable_type() {
        let select = bind_select_orders("SELECT MIN(dept), MAX(dept) FROM orders");
        let aggregate = select.aggregate.unwrap();
        assert_eq!(aggregate.schema.columns()[0].data_type, DataType::Text);
    }

    #[test]
    fn count_star_requires_no_argument_but_others_do() {
        // `Parser`がすでに`SUM(*)`を構文エラーとして拒否する(この章の構文検査)。
        let statement = crate::parser::parse_statement("SELECT SUM(*) FROM orders");
        assert!(statement.is_err());
    }

    #[test]
    fn order_by_matching_the_select_list_resolves_to_that_column() {
        let select = bind_select_orders("SELECT dept FROM orders ORDER BY dept DESC");
        assert_eq!(select.order_by.len(), 1);
        assert!(select.order_by[0].desc);
        match &select.order_by[0].expr {
            BoundExpr::ColumnRef { table_ordinal, column_index, .. } => {
                assert_eq!(*table_ordinal, 0);
                assert_eq!(*column_index, 0);
            }
            other => panic!("ColumnRefを期待したが{other:?}"),
        }
    }

    #[test]
    fn order_by_referencing_a_column_outside_the_select_list_adds_a_hidden_column() {
        // `SELECT name FROM t ORDER BY id`という最頻出パターン。`id`は`SELECT`の
        // 対象式には無いが、`projection`の末尾に隠し列として追加され、束縛自体は
        // 成功する。
        let select = bind_select_orders("SELECT dept FROM orders ORDER BY amount");
        assert_eq!(select.hidden_column_count, 1);
        assert_eq!(select.projection.len(), 2);
        assert_eq!(select.projection[1].output_name, "amount");
        match &select.order_by[0].expr {
            BoundExpr::ColumnRef { column_index, .. } => assert_eq!(*column_index, 1),
            other => panic!("ColumnRefを期待したが{other:?}"),
        }
    }

    #[test]
    fn order_by_matching_an_existing_projection_item_does_not_add_a_hidden_column() {
        // 「射影に同名の出力列があればそれが優先される」という優先順位。
        let select = bind_select_orders("SELECT dept FROM orders ORDER BY dept");
        assert_eq!(select.hidden_column_count, 0);
        assert_eq!(select.projection.len(), 1);
        match &select.order_by[0].expr {
            BoundExpr::ColumnRef { column_index, .. } => assert_eq!(*column_index, 0),
            other => panic!("ColumnRefを期待したが{other:?}"),
        }
    }

    #[test]
    fn order_by_can_write_a_fresh_aggregate_call_not_in_the_select_list() {
        // `ORDER BY COUNT(*) DESC`のように、`SELECT`の対象式に無い集約呼び出しも
        // 隠し列として`Aggregate`の出力へ追加される。
        let select = bind_select_orders("SELECT dept FROM orders GROUP BY dept ORDER BY COUNT(*) DESC");
        let aggregate = select.aggregate.unwrap();
        assert_eq!(aggregate.calls.len(), 1);
        assert_eq!(select.hidden_column_count, 1);
        assert!(select.order_by[0].desc);
    }

    #[test]
    fn order_by_aggregate_already_in_select_list_does_not_add_a_hidden_column() {
        let select = bind_select_orders("SELECT dept, COUNT(*) FROM orders GROUP BY dept ORDER BY COUNT(*) DESC");
        assert_eq!(select.hidden_column_count, 0);
        assert_eq!(select.projection.len(), 2);
        match &select.order_by[0].expr {
            BoundExpr::ColumnRef { column_index, .. } => assert_eq!(*column_index, 1),
            other => panic!("ColumnRefを期待したが{other:?}"),
        }
    }

    #[test]
    fn order_by_introducing_an_aggregate_promotes_the_query_to_aggregate_even_without_group_by() {
        // `GROUP BY`も`HAVING`も無い`SELECT`でも、`ORDER BY`に集約関数が
        // 現れた時点でこの`SELECT`全体が集約クエリになる。グループ化キーの
        // 無い集約に対しては、テーブル全体が1個のグループになるため、
        // グループ化されていない`dept`列を射影に含めることはできない
        // (関数従属性の違反)。
        let err = bind_err_orders("SELECT dept FROM orders ORDER BY COUNT(*) DESC");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn order_by_with_distinct_cannot_add_a_hidden_column() {
        // `DISTINCT`を伴う`SELECT`は、隠し列を経由した`ORDER BY`を許さない
        // (どの`amount`の値を残すかが、`DISTINCT`で行が1つに畳まれた時点で
        // 定まらなくなるため、PostgreSQLと同じ制約を採用する)。
        let err = bind_err_orders("SELECT DISTINCT dept FROM orders ORDER BY amount");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn order_by_with_distinct_matching_the_select_list_is_still_allowed() {
        let select = bind_select_orders("SELECT DISTINCT dept FROM orders ORDER BY dept");
        assert_eq!(select.hidden_column_count, 0);
    }

    #[test]
    fn limit_and_offset_are_evaluated_to_constants_at_bind_time() {
        let select = bind_select_orders("SELECT dept FROM orders LIMIT 1 + 1 OFFSET 3");
        assert_eq!(select.limit, Some(2));
        assert_eq!(select.offset, Some(3));
    }

    #[test]
    fn negative_limit_is_rejected() {
        let err = bind_err_orders("SELECT dept FROM orders LIMIT -1");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn limit_referencing_a_column_is_rejected() {
        let err = bind_err_orders("SELECT dept FROM orders LIMIT amount");
        assert!(matches!(err, DbError::Bind { .. }));
    }

    #[test]
    fn distinct_flag_is_propagated() {
        let select = bind_select_orders("SELECT DISTINCT dept FROM orders");
        assert!(select.distinct);
        let select = bind_select_orders("SELECT dept FROM orders");
        assert!(!select.distinct);
    }
}
