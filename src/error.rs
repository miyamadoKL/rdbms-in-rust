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

    /// SQL文字列を構文解析できなかったエラー。発生位置の行・列を持つ。
    ///
    /// `Lex`と表示形式を揃えている(`行N列M: 種別: メッセージ`)。字句解析までは
    /// 成功したが、Token列がこのSQLサブセットの文法に適合しない場合に返す。
    #[error("行{line}列{column}: 構文エラー: {message}")]
    Parse {
        /// エラーの内容。
        message: String,
        /// 発生位置の行番号(1始まり)。
        line: usize,
        /// 発生位置の列番号(1始まり)。
        column: usize,
    },

    /// SQL文字列をToken列へ変換できなかったエラー。発生位置の行・列を持つ。
    #[error("行{line}列{column}: 字句エラー: {message}")]
    Lex {
        /// エラーの内容。
        message: String,
        /// 発生位置の行番号(1始まり)。
        line: usize,
        /// 発生位置の列番号(1始まり)。
        column: usize,
    },
}

/// minidb の操作全般で使う `Result` エイリアス。
pub type DbResult<T> = Result<T, DbError>;
