//! minidb: Rustで自作するRDBMSのコアクレート。
//!
//! 教材の各章はこのクレートを段階的に育てていく。現時点では
//! エラー型、識別子のNewtype、簡易ログマクロ、関係モデルの型、
//! SQL文字列をToken列へ変換する字句解析器(`lexer`)、Token列をASTへ変換する
//! 構文解析器(`parser`、`ast`)、`Expr`を`Value`へ変換する式評価器(`eval`)に加えて、
//! `SELECT`の式リストを実行できる最初の縦切り(`Database`)を提供する。

pub mod ast;
pub mod database;
pub mod error;
pub mod eval;
pub mod ids;
pub mod lexer;
pub mod parser;
pub mod types;

pub use ast::{Expr, Statement};
pub use database::{Database, QueryResult};
pub use error::{DbError, DbResult};
pub use eval::{FunctionRegistry, eval_expr};
pub use ids::{PageId, TableId, TransactionId};
pub use lexer::{Keyword, Span, Token, TokenKind, tokenize};
pub use parser::parse_statement;
pub use types::{Column, DataType, Schema, Tuple, Value};

/// 簡易ログ出力マクロ(依存追加を避けるため `eprintln!` を薄くラップするだけ)。
///
/// ```
/// minidb::log_info!("starting {}", 1);
/// ```
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        eprintln!("[minidb] {}", format_args!($($arg)*))
    };
}
