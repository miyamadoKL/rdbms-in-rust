//! minidb: Rustで自作するRDBMSのコアクレート。
//!
//! 教材の各章はこのクレートを段階的に育てていく。現時点では
//! エラー型と識別子のNewtypeのみを提供する骨格。

pub mod error;
pub mod ids;

pub use error::{DbError, DbResult};
pub use ids::{PageId, TableId, TransactionId};

/// 簡易ログ出力マクロ(依存追加を避けるため `eprintln!` を薄くラップするだけ)。
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        eprintln!("[minidb] {}", format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_id_equality() {
        assert_eq!(PageId(1), PageId(1));
        assert_ne!(PageId(1), PageId(2));
    }
}
