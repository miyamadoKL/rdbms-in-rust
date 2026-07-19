//! 構文解析器(`parser`)が組み立てる抽象構文木(AST)。
//!
//! ここで表すのは「SQL文字列がどういう構造を持つか」だけである。`users`という
//! 識別子が実在するテーブルを指すか、`id`が`users`の列として存在するかといった
//! 名前解決や型検査は行わない。テーブル・列の定義を保持するカタログは第9章で
//! 作るが、名前をカタログと突き合わせる`Binder`は第17章で導入する。
//! ASTはBinderが名前解決の前に読む「構文だけを確定させた中間表現」に留める。
//!
//! 各ノードは`Span`(ソース中のバイト範囲)を持つ。パニックせずに構文エラーの
//! 位置を報告するのは第6章のLexerと同じ不変条件であり、AST側もこの範囲を
//! 保持することで、後段(構文エラー・Binderのエラー・EXPLAINでの位置表示)が
//! 常に元のSQL文字列へたどり着けるようにする。

use crate::lexer::Span;

/// SQL文1本を表す。
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `SELECT`文。第21章で`ORDER BY`・`GROUP BY`・`HAVING`・`LIMIT`・`OFFSET`が
    /// 加わり`SelectStatement`自体が大きくなったため、他の(小さい)variantとの
    /// サイズ差を抑えるために`Box`で間接化する。
    Select(Box<SelectStatement>),
    /// `CREATE TABLE`文。
    CreateTable(CreateTableStatement),
    /// `DROP TABLE`文。
    DropTable(DropTableStatement),
    /// `CREATE INDEX` / `CREATE UNIQUE INDEX`文(第24章)。
    CreateIndex(CreateIndexStatement),
    /// `DROP INDEX`文(第24章)。
    DropIndex(DropIndexStatement),
    /// `INSERT INTO`文。
    Insert(InsertStatement),
    /// `UPDATE`文。
    Update(UpdateStatement),
    /// `DELETE FROM`文。
    Delete(DeleteStatement),
    /// `EXPLAIN [ANALYZE]`文。`SELECT`・`INSERT INTO`・`UPDATE`・`DELETE FROM`の
    /// いずれか1本を対象に取れる(第19章)。`CREATE TABLE`・`DROP TABLE`は
    /// Logical Plan/Physical Planを経由しない文であり、`EXPLAIN`する対象を
    /// 持たないため対象に含めない。`ANALYZE`修飾(第27章)は
    /// `ExplainStatement::analyze`が持つ。
    Explain(ExplainStatement),
    /// `ANALYZE [テーブル名]`文(第27章)。統計情報を収集する。テーブル名を
    /// 省略した場合はカタログに登録されている全テーブルが対象になる。
    Analyze(AnalyzeStatement),
    /// `BEGIN`文(第30章)。明示的なトランザクションを開始する。
    Begin(BeginStatement),
    /// `COMMIT`文(第30章)。現在のトランザクションの変更を確定する。
    Commit(CommitStatement),
    /// `ROLLBACK`文(第30章)。現在のトランザクションの変更を取り消す。
    Rollback(RollbackStatement),
    /// `CHECKPOINT`文(第34章)。全dirtyページをflush・syncし、Checkpoint
    /// レコードをWALへ書く。
    Checkpoint(CheckpointStatement),
}

impl Statement {
    /// この文がソース中で占める範囲。
    pub fn span(&self) -> Span {
        match self {
            Statement::Select(s) => s.span,
            Statement::CreateTable(s) => s.span,
            Statement::DropTable(s) => s.span,
            Statement::CreateIndex(s) => s.span,
            Statement::DropIndex(s) => s.span,
            Statement::Insert(s) => s.span,
            Statement::Update(s) => s.span,
            Statement::Delete(s) => s.span,
            Statement::Explain(s) => s.span,
            Statement::Analyze(s) => s.span,
            Statement::Begin(s) => s.span,
            Statement::Commit(s) => s.span,
            Statement::Rollback(s) => s.span,
            Statement::Checkpoint(s) => s.span,
        }
    }
}

/// 引用符で囲まれていない識別子。テーブル名・列名・関数名になる。
///
/// `Ident`はテキストをそのまま保持し、大文字小文字の畳み込みはしない。
/// この方針は第6章のLexerが`TokenKind::Ident`について決めたものをそのまま引き継ぐ。
#[derive(Debug, Clone, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

/// `SELECT`文。
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    /// `DISTINCT`が指定されていたかどうか(第21章)。
    pub distinct: bool,
    /// `SELECT`の直後に並ぶ、カンマ区切りの式リスト。
    pub items: Vec<SelectItem>,
    /// `FROM <table> [AS <alias>]`。省略した`SELECT`は、列を持たない空のSchemaに
    /// 対する1件のタプルを暗黙の入力とみなして実行する(`where_clause`のドキュメント
    /// コメント参照)。
    pub from: Option<FromClause>,
    /// `WHERE <expr>`。`from`を伴わない`SELECT`でも構文として受理するだけでなく、
    /// 意味も持つ。`from`が無い`SELECT`は、この1件の暗黙のタプルに対して
    /// `where_clause`を適用し、`TRUE`なら1行、`FALSE`または`NULL`(UNKNOWN)なら
    /// 0行を返す(`database`モジュールの`execute_select_without_from`参照)。
    pub where_clause: Option<Expr>,
    /// `GROUP BY <式, ...>`(第21章)。空なら`GROUP BY`を持たない。
    pub group_by: Vec<Expr>,
    /// `HAVING <expr>`(第21章)。
    pub having: Option<Expr>,
    /// `ORDER BY <式> [ASC|DESC], ...`(第21章)。
    pub order_by: Vec<OrderByItem>,
    /// `LIMIT <expr>`(第21章)。
    pub limit: Option<Expr>,
    /// `OFFSET <expr>`(第21章)。`LIMIT`を伴わない`OFFSET`単体も構文としては許す。
    pub offset: Option<Expr>,
    pub span: Span,
}

/// `ORDER BY`に並ぶ要素1個(第21章)。
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    /// `DESC`が指定されていたかどうか。`ASC`または未指定なら`false`。
    pub desc: bool,
    pub span: Span,
}

/// 集約関数の種類(第21章)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Min,
    Max,
}

impl AggregateFunc {
    /// 識別子(大文字小文字を無視)を集約関数として解決する。集約関数でなければ`None`。
    pub fn from_name(name: &str) -> Option<AggregateFunc> {
        match name.to_ascii_uppercase().as_str() {
            "COUNT" => Some(AggregateFunc::Count),
            "SUM" => Some(AggregateFunc::Sum),
            "MIN" => Some(AggregateFunc::Min),
            "MAX" => Some(AggregateFunc::Max),
            _ => None,
        }
    }

    /// SQLの関数名としての表示(常に大文字)。
    pub fn name(self) -> &'static str {
        match self {
            AggregateFunc::Count => "COUNT",
            AggregateFunc::Sum => "SUM",
            AggregateFunc::Min => "MIN",
            AggregateFunc::Max => "MAX",
        }
    }
}

/// `SELECT`の`FROM <table> [AS <alias>] [<JOIN> ...]`。
///
/// `alias`があれば、`table`自身の名前は`Binder`(第17章)による列参照の解決では
/// 使えなくなる(`AS`はテーブルを新しい名前で覆い隠す、標準SQLの規則)。
/// `alias`が無い場合は`table`の名前がそのまま修飾子として使える。
///
/// `joins`は、`table`の右側へ順に連結する`INNER JOIN`の並び(第22章)。
/// 空なら単一テーブルの`FROM`であり、この場合は第17章までと同じ意味を持つ。
/// カンマ区切りの複数テーブル(`FROM a, b`)はこの章では構文として受理しない
/// (`parser`モジュールのドキュメント、および本文の解説を参照)。
#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    pub table: Ident,
    pub alias: Option<Ident>,
    pub joins: Vec<JoinClause>,
    pub span: Span,
}

/// `JOIN`の種類(第22章)。この章では`INNER`(`JOIN`単独も同義)のみを扱う。
/// `LEFT OUTER JOIN`等は章末の演習課題で追加する対象として、あえて
/// バリアントを1つだけに絞ってある。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
}

impl JoinKind {
    /// `EXPLAIN`・エラーメッセージでの表示名。
    pub fn name(self) -> &'static str {
        match self {
            JoinKind::Inner => "INNER JOIN",
        }
    }
}

/// `FROM`に続く1個の`[INNER] JOIN <table> [AS <alias>] ON <expr>`(第22章)。
#[derive(Debug, Clone, PartialEq)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub table: Ident,
    pub alias: Option<Ident>,
    /// `ON`に続く結合条件。`WHERE`と同じくBOOLEANを返す式でなければならない
    /// (`Binder::bind_from`が検査する)。
    pub on: Expr,
    pub span: Span,
}

/// `SELECT`の対象式リストに並ぶ要素1個。
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// 通常の式。
    Expr { expr: Expr, span: Span },
    /// `*`。`FROM`で指定したテーブルの全列に展開される。
    Wildcard { span: Span },
}

impl SelectItem {
    /// この要素がソース中で占める範囲。
    pub fn span(&self) -> Span {
        match self {
            SelectItem::Expr { span, .. } | SelectItem::Wildcard { span } => *span,
        }
    }
}

/// `CREATE TABLE`文。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTableStatement {
    pub table: Ident,
    pub columns: Vec<ColumnDef>,
    pub span: Span,
}

/// `CREATE TABLE`の列定義1個。
///
/// `type_name`は`BIGINT`のような型名をテキストのまま保持する。これを
/// `crate::types::DataType`へ解決するのは第9章のカタログの仕事であり、
/// この章の`Parser`はまだ型名の一覧を知らない(知る必要もない)。
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: Ident,
    pub type_name: Ident,
    /// `NOT NULL`が指定されていたかどうか。
    pub not_null: bool,
    /// `PRIMARY KEY`が指定されていたかどうか(第20章)。単一列のみに対応し、
    /// 同じ`CREATE TABLE`の複数の列に指定された場合の扱いは`Database`側
    /// (`execute_create_table`)が検査する。
    pub primary_key: bool,
    /// `UNIQUE`が指定されていたかどうか(第20章)。
    pub unique: bool,
    pub span: Span,
}

/// `DROP TABLE`文。
#[derive(Debug, Clone, PartialEq)]
pub struct DropTableStatement {
    pub table: Ident,
    pub span: Span,
}

/// `CREATE INDEX <index> ON <table> (<column>)`文(第24章)。
///
/// このSQLサブセットの索引キーは単一列に限る(`crate::btree`と同じ制約)ため、
/// `(<column>)`の中は列名を1個だけ持つ。`UNIQUE`が指定されていれば`unique`が
/// `true`になり、`PRIMARY KEY`・`UNIQUE`列に自動で作られる索引(`Database::execute_create_table`)と
/// 同じ、キーの重複を`crate::btree::BTree`自身が拒否する索引になる。
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndexStatement {
    pub unique: bool,
    pub index: Ident,
    pub table: Ident,
    pub column: Ident,
    pub span: Span,
}

/// `DROP INDEX`文(第24章)。
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndexStatement {
    pub index: Ident,
    pub span: Span,
}

/// `INSERT INTO`文。
///
/// `INSERT INTO name VALUES (...), (...), ...`という複数行の挿入と、
/// `INSERT INTO name (col, ...) VALUES (...)`という列名の明示の両方に対応する。
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStatement {
    pub table: Ident,
    /// 明示された列名。`None`なら位置で対応させる(`VALUES`内の各行が、
    /// テーブルの列と同じ個数・同じ並び順であることを前提にする)。
    pub columns: Option<Vec<Ident>>,
    /// `VALUES`に続く行の並び。1行に満たない、あるいは超える個数の式を
    /// 持つ行があっても構文解析の時点では検査せず、実行時に検査する。
    pub rows: Vec<Vec<Expr>>,
    pub span: Span,
}

/// `UPDATE`文。
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateStatement {
    pub table: Ident,
    /// `SET`に続く、カンマ区切りの代入リスト。
    pub assignments: Vec<Assignment>,
    /// `WHERE <expr>`。省略した場合はテーブルの全行が対象になる。
    pub where_clause: Option<Expr>,
    pub span: Span,
}

/// `UPDATE`の`SET`リストに並ぶ`<column> = <expr>`1個。
#[derive(Debug, Clone, PartialEq)]
pub struct Assignment {
    pub column: Ident,
    pub value: Expr,
    pub span: Span,
}

/// `DELETE FROM`文。
#[derive(Debug, Clone, PartialEq)]
pub struct DeleteStatement {
    pub table: Ident,
    /// `WHERE <expr>`。省略した場合はテーブルの全行が対象になる。
    pub where_clause: Option<Expr>,
    pub span: Span,
}

/// `EXPLAIN [ANALYZE]`文。`statement`は`EXPLAIN`(または`EXPLAIN ANALYZE`)の
/// 直後に続く1本のSQL文。
#[derive(Debug, Clone, PartialEq)]
pub struct ExplainStatement {
    pub statement: Box<Statement>,
    /// `ANALYZE`修飾があったかどうか(第27章)。`false`なら推定行数だけを
    /// 表示する従来の`EXPLAIN`、`true`なら実際に実行して実測行数も
    /// 併記する`EXPLAIN ANALYZE`(PostgreSQLの`EXPLAIN ANALYZE`に相当)。
    pub analyze: bool,
    /// `EXPLAIN`キーワードから対象の文の末尾までを覆う範囲。
    pub span: Span,
}

/// `ANALYZE [テーブル名]`文(第27章)。
#[derive(Debug, Clone, PartialEq)]
pub struct AnalyzeStatement {
    /// 対象テーブル名。`None`ならカタログに登録されている全テーブルが対象。
    pub table: Option<Ident>,
    pub span: Span,
}

/// `BEGIN`文(第30章)。`BEGIN TRANSACTION`のような修飾は持たず、`BEGIN`
/// 単体、または`BEGIN ISOLATION LEVEL <level>`(第32章)だけを受理する
/// (`docs-local/chatgpt_opinion.md`の原案が挙げる`BEGIN TRANSACTION`・
/// `BEGIN WORK`のような修飾語は、この章でも引き続き受理しない)。
#[derive(Debug, Clone, PartialEq)]
pub struct BeginStatement {
    /// 省略した場合は`None`になり、`Database::execute_begin`が既定の
    /// 分離レベル(Repeatable Read、本文「分離レベルの既定値」を参照)を補う。
    pub isolation_level: Option<IsolationLevel>,
    pub span: Span,
}

/// `BEGIN ISOLATION LEVEL ...`(第32章)が指定できる4つの分離レベル。
///
/// SQL標準が定める順序どおりに並べてある(`Read Uncommitted`が最も緩く、
/// `Serializable`が最も厳格)。この型はASTの一部であり、`Binder`を経由せず
/// そのまま`crate::transaction::TransactionContext`へ渡る(`BeginStatement`
/// 自体が`Binder`をほぼ素通りするのと同じ理由、本文を参照)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// `READ UNCOMMITTED`。読み取りロックを一切取らない。
    ReadUncommitted,
    /// `READ COMMITTED`。読み取りロックを文の終わりで解放する。
    ReadCommitted,
    /// `REPEATABLE READ`。読み取りロックもCOMMITまで保持する(第31章の
    /// Strict 2PLがもともと持っていた挙動そのもの)。
    RepeatableRead,
    /// `SERIALIZABLE`。`RepeatableRead`に加え、Phantomも防ぐ。
    Serializable,
}

/// `COMMIT`文(第30章)。
#[derive(Debug, Clone, PartialEq)]
pub struct CommitStatement {
    pub span: Span,
}

/// `ROLLBACK`文(第30章)。
#[derive(Debug, Clone, PartialEq)]
pub struct RollbackStatement {
    pub span: Span,
}

/// `CHECKPOINT`文(第34章)。修飾語を持たない単体の文。
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointStatement {
    pub span: Span,
}

/// 二項演算子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add,
    Subtract,
    Multiply,
    Divide,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

/// 単項演算子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    /// `-x`
    Negate,
    /// `NOT x`
    Not,
}

/// SQLの式。
///
/// 対応するのはリテラル、列参照、二項演算、単項演算、`IS [NOT] NULL`、
/// 関数呼び出し、括弧による優先順位の明示のみ。列参照(`ColumnRef`)は
/// 名前を保持するだけで、それがどのテーブルのどの列を指すかの解決は行わない。
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
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
    /// 列参照。`users.id`のような修飾名も、`qualifier`に`users`を持つことで
    /// 表現できる(第17章で`Dot`トークンに対応した)。`qualifier`がテーブル名
    /// そのものを指すかテーブルAliasを指すかはASTの時点では区別せず、
    /// どちらの解決(`Binder`)も同じ`Ident`から行う。
    ColumnRef {
        qualifier: Option<Ident>,
        name: String,
        span: Span,
    },
    UnaryOp {
        op: UnaryOperator,
        expr: Box<Expr>,
        span: Span,
    },
    BinaryOp {
        op: BinaryOperator,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    /// `<expr> IS [NOT] NULL`。
    IsNull {
        expr: Box<Expr>,
        negated: bool,
        span: Span,
    },
    /// `name(arg, arg, ...)`。引数0個の`name()`も許す。
    FunctionCall {
        name: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// 集約関数呼び出し(第21章)。`COUNT`・`SUM`・`MIN`・`MAX`は、Scalar
    /// Functionとは異なり複数行にまたがる状態(集計中の合計・件数など)を
    /// 持つため、`FunctionCall`とは別のノードとして表す。`arg`が`None`なのは
    /// `COUNT(*)`だけであり、他の3関数は構文の時点で必ず引数を1個持つ
    /// (`Parser::parse_aggregate_call`が検査する)。
    Aggregate {
        func: AggregateFunc,
        arg: Option<Box<Expr>>,
        span: Span,
    },
    /// `(expr)`。優先順位を明示するための括弧そのものをASTに残す。
    Paren {
        expr: Box<Expr>,
        span: Span,
    },
    /// `CAST(expr AS type_name)`。`type_name`はColumnDefの`type_name`と同様、
    /// 型名の一覧を知らないParserがテキストのまま保持する。`crate::types::DataType`
    /// への解決は評価器(第8章の`eval`モジュール)の仕事にする。
    Cast {
        expr: Box<Expr>,
        type_name: Ident,
        span: Span,
    },
}

impl Expr {
    /// この式がソース中で占める範囲。括弧で囲まれた式は、括弧自体を含む範囲になる。
    pub fn span(&self) -> Span {
        match self {
            Expr::IntLiteral { span, .. }
            | Expr::StringLiteral { span, .. }
            | Expr::BoolLiteral { span, .. }
            | Expr::NullLiteral { span }
            | Expr::ColumnRef { span, .. }
            | Expr::UnaryOp { span, .. }
            | Expr::BinaryOp { span, .. }
            | Expr::IsNull { span, .. }
            | Expr::FunctionCall { span, .. }
            | Expr::Aggregate { span, .. }
            | Expr::Paren { span, .. }
            | Expr::Cast { span, .. } => *span,
        }
    }
}
