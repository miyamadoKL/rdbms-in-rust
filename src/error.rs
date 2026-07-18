//! minidb 全体で使うエラー型。
//!
//! 各章で機能が増えるにつれて variant を追加していく想定の骨格。

use thiserror::Error;

/// minidb の操作全般で返されるエラー。
#[derive(Debug, Error)]
pub enum DbError {
    /// I/O 由来のエラー(ファイル読み書き等)。
    #[error("I/Oエラー: {0}")]
    Io(#[from] std::io::Error),

    /// まだ実装されていない機能を呼び出したときのエラー。
    #[error("未実装: {0}")]
    NotImplemented(String),

    /// 値の並びがSchemaの列数・型・nullable制約に適合しないエラー。
    #[error("スキーマ不一致: {0}")]
    SchemaMismatch(String),
}

/// minidb の操作全般で使う `Result` エイリアス。
pub type DbResult<T> = Result<T, DbError>;
