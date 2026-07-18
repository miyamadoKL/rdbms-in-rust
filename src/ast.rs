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
    /// `SELECT`文。
    Select(SelectStatement),
    /// `CREATE TABLE`文。
    CreateTable(CreateTableStatement),
    /// `DROP TABLE`文。
    DropTable(DropTableStatement),
    /// `INSERT INTO`文。
    Insert(InsertStatement),
    /// `UPDATE`文。
    Update(UpdateStatement),
    /// `DELETE FROM`文。
    Delete(DeleteStatement),
    /// `EXPLAIN`文。`SELECT`・`INSERT INTO`・`UPDATE`・`DELETE FROM`のいずれか
    /// 1本を対象に取れる(第19章)。`CREATE TABLE`・`DROP TABLE`はLogical
    /// Plan/Physical Planを経由しない文であり、`EXPLAIN`する対象を持たないため
    /// 対象に含めない。
    Explain(ExplainStatement),
}

impl Statement {
    /// この文がソース中で占める範囲。
    pub fn span(&self) -> Span {
        match self {
            Statement::Select(s) => s.span,
            Statement::CreateTable(s) => s.span,
            Statement::DropTable(s) => s.span,
            Statement::Insert(s) => s.span,
            Statement::Update(s) => s.span,
            Statement::Delete(s) => s.span,
            Statement::Explain(s) => s.span,
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
    pub span: Span,
}

/// `SELECT`の`FROM <table> [AS <alias>]`。
///
/// `alias`があれば、`table`自身の名前は`Binder`(第17章)による列参照の解決では
/// 使えなくなる(`AS`はテーブルを新しい名前で覆い隠す、標準SQLの規則)。
/// `alias`が無い場合は`table`の名前がそのまま修飾子として使える。
#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    pub table: Ident,
    pub alias: Option<Ident>,
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

/// `EXPLAIN`文。`statement`は`EXPLAIN`の直後に続く1本のSQL文。
#[derive(Debug, Clone, PartialEq)]
pub struct ExplainStatement {
    pub statement: Box<Statement>,
    /// `EXPLAIN`キーワードから対象の文の末尾までを覆う範囲。
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
            | Expr::Paren { span, .. }
            | Expr::Cast { span, .. } => *span,
        }
    }
}
