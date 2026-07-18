//! minidb: Rustで自作するRDBMSのコアクレート。
//!
//! 教材の各章はこのクレートを段階的に育てていく。現時点では
//! エラー型、識別子のNewtype、簡易ログマクロ、関係モデルの型、
//! SQL文字列をToken列へ変換する字句解析器(`lexer`)、Token列をASTへ変換する
//! 構文解析器(`parser`、`ast`)、行環境(`Row`)を伴って`Expr`を`Value`へ変換する
//! 式評価器(`eval`)、テーブル定義の唯一の情報源であるインメモリカタログ
//! (`catalog`)、テーブルの行そのものを保持するインメモリストレージ
//! (`storage_mem`)、Sequential Scan・Filter・Projection・Insert・Update・
//! Deleteの各演算子(`executor`)に加えて、`SELECT`・`INSERT`・`UPDATE`・
//! `DELETE`・`CREATE TABLE`・`DROP TABLE`を実行できる縦切り(`Database`)を
//! 提供する。第2部からは、データベースファイルのオンディスク形式(File Header、
//! Page、Checksum)を扱う`page`、Pageの`payload`内に可変長レコードを詰める
//! Slotted Page(`slotted_page`)と、`Tuple`をそのバイト列との間でencode/decode
//! する`tuple_codec`、`Page`を実ファイルへ読み書きする`disk_manager`、複数の
//! ページをまとめて1つのテーブルとして扱う`heap_file`、`disk_manager`の上に
//! 固定容量のページキャッシュを置く`buffer_pool`が加わる。第15章では、
//! ページごとの空き容量の見積もりを保持する`free_space_map`と、テーブル定義
//! そのものを1つのファイルへ永続化し、複数のテーブルを1つのファイルに同居させる
//! ストレージエンジン`storage`が加わる。

pub mod ast;
pub mod buffer_pool;
pub mod catalog;
pub mod database;
pub mod disk_manager;
pub mod error;
pub mod eval;
pub mod executor;
pub mod free_space_map;
pub mod heap_file;
pub mod ids;
pub mod lexer;
pub mod page;
pub mod parser;
pub mod slotted_page;
pub mod storage;
pub mod storage_mem;
pub mod tuple_codec;
pub mod types;

pub use ast::{Expr, Statement};
pub use buffer_pool::{BufferPool, BufferPoolStats, PageReadGuard, PageWriteGuard};
pub use catalog::{Catalog, TableInfo};
pub use database::{Database, QueryResult};
pub use disk_manager::DiskManager;
pub use error::{DbError, DbResult};
pub use eval::{FunctionRegistry, eval_expr};
pub use free_space_map::FreeSpaceMap;
pub use heap_file::{HeapFile, Scan};
pub use ids::{PageId, RecordId, SlotId, TableId, TransactionId};
pub use lexer::{Keyword, Span, Token, TokenKind, tokenize};
pub use page::{
    FILE_HEADER_SIZE, FORMAT_VERSION, MAGIC, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE,
    FileHeader, Page, PageType,
};
pub use parser::parse_statement;
pub use slotted_page::{SLOT_ENTRY_SIZE, SLOTTED_HEADER_SIZE, SlotStatus, SlottedPage, SlottedPageRef};
pub use storage::Storage;
pub use storage_mem::{MemStorage, MemTable};
pub use tuple_codec::{decode_tuple, encode_tuple};
pub use types::{Column, DataType, Row, Schema, Tuple, Value};

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
