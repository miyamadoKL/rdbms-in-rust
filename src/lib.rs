//! minidb: Rustで自作するRDBMSのコアクレート。
//!
//! 教材の各章はこのクレートを段階的に育てていく。現時点では
//! エラー型、識別子のNewtype、簡易ログマクロを提供する骨格。

pub mod error;
pub mod ids;

pub use error::{DbError, DbResult};
pub use ids::{PageId, TableId, TransactionId};

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
