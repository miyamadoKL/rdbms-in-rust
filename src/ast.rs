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
    /// `INSERT INTO`文。
    Insert(InsertStatement),
}

impl Statement {
    /// この文がソース中で占める範囲。
    pub fn span(&self) -> Span {
        match self {
            Statement::Select(s) => s.span,
            Statement::CreateTable(s) => s.span,
            Statement::Insert(s) => s.span,
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
///
/// `FROM`と`WHERE`は構文として受理するが、実行できるのは第9〜10章でカタログと
/// インメモリ表が揃ってからになる(`Database::execute`は`NotImplemented`を返す)。
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStatement {
    /// `SELECT`の直後に並ぶ、カンマ区切りの式リスト。
    pub items: Vec<SelectItem>,
    /// `FROM <table>`。第9〜10章まではカタログが無いため実行できない。
    pub from: Option<Ident>,
    /// `WHERE <expr>`。`from`と同様、実行は第9〜10章以降。
    pub where_clause: Option<Expr>,
    pub span: Span,
}

/// `SELECT`の対象式リストに並ぶ要素1個。
#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    pub expr: Expr,
    pub span: Span,
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
    pub span: Span,
}

/// `INSERT INTO`文。
///
/// 対応するのは`INSERT INTO name VALUES (...)`という1行分の挿入のみ。
/// 複数行の`VALUES (...), (...)`や列名の明示(`INSERT INTO name (col, ...)`)は
/// 対象外とし、必要になった章(第10章)で拡張する。
#[derive(Debug, Clone, PartialEq)]
pub struct InsertStatement {
    pub table: Ident,
    pub values: Vec<Expr>,
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
    /// 列参照。`users.id`のような修飾名は、Lexerが`.`を扱わないため対象外。
    ColumnRef {
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
            | Expr::Paren { span, .. } => *span,
        }
    }
}
