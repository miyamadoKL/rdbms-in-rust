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

    /// `CREATE TABLE`が、すでにカタログへ登録済みのテーブル名を指定したエラー。
    #[error("テーブルはすでに存在します: {0}")]
    DuplicateTable(String),

    /// `CREATE TABLE`の列定義に、同じ列名が2回以上出てきたエラー。
    #[error("列名が重複しています: {0}")]
    DuplicateColumn(String),

    /// `DROP TABLE`が、カタログに登録されていないテーブル名を指定したエラー。
    #[error("テーブルが存在しません: {0}")]
    TableNotFound(String),

    /// 式の評価に失敗したエラー(型不一致、ゼロ除算、整数オーバーフロー、
    /// `CAST`の失敗、未知の関数・型名など)。
    #[error("評価エラー: {0}")]
    Eval(String),

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

    /// File HeaderまたはPageのバイト列が壊れているエラー(Magic Number不一致、
    /// Format Version不一致、checksum不一致、バイト数不一致、未知のPage Typeなど)。
    #[error("破損したページです: {0}")]
    CorruptPage(String),

    /// Tupleのバイト列が、渡された`Schema`のもとで復元できないエラー
    /// (バイト列がNULLビットマップや値の途中で尽きている、`TEXT`の長さプレフィックス
    /// が実際の残りバイト数を超えているなど)。
    #[error("破損したタプルです: {0}")]
    CorruptTuple(String),

    /// `DiskManager`に、まだ`allocate_page`されていない(または`page_count`の
    /// 範囲外の)`PageId`を渡したエラー。
    #[error("ページ範囲外です: {0}")]
    PageOutOfRange(String),

    /// `HeapFile::insert`に渡したバイト列が、空の1ページにも収まらないほど
    /// 大きいエラー。
    #[error("挿入するデータがページに収まりません: {0}バイト")]
    TupleTooLarge(usize),

    /// `BufferPool`が新しいページを読み込もうとしたが、既存の全フレームがpin中で
    /// evictできる候補が1つもないエラー。
    #[error("バッファプールの全フレームがpin中です: {0}")]
    BufferPoolFull(String),
}

/// minidb の操作全般で使う `Result` エイリアス。
pub type DbResult<T> = Result<T, DbError>;
