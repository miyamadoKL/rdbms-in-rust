//! minidb: Rustで自作するRDBMSのコアクレート。
//!
//! 教材の各章はこのクレートを段階的に育てていく。現時点では
//! エラー型、識別子のNewtype、簡易ログマクロ、関係モデルの型、
//! SQL文字列をToken列へ変換する字句解析器(`lexer`)に加えて、
//! `SELECT <式>`を実行できる最初の縦切り(`Database`)を提供する。

pub mod database;
pub mod error;
pub mod ids;
pub mod lexer;
mod toy_sql;
pub mod types;

pub use database::{Database, QueryResult};
pub use error::{DbError, DbResult};
pub use ids::{PageId, TableId, TransactionId};
pub use lexer::{Keyword, Span, Token, TokenKind, tokenize};
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
