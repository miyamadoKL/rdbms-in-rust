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
//! ストレージエンジン`storage`が加わる。第16章では、`database`のSQL実行経路が
//! `storage`(永続モード、`Database::open`)と`storage_mem`(インメモリモード、
//! `Database::memory`)のどちらでも動くようになり、`CREATE TABLE`・`INSERT`・
//! `SELECT`・`UPDATE`・`DELETE`が再起動をまたいで残る。第17章では、ASTを
//! カタログと突き合わせて名前解決・型検査を行う`Binder`が加わり、`database`の
//! 実行経路は構文解析(`parser`)→名前解決(`binder`)→実行(`executor`)という
//! 3段階になる。第18章では、`BoundStatement`を関係代数の演算子木
//! (`LogicalPlan`)へ変換する`logical_plan`が加わり、`database`の実行経路は
//! 構文解析→名前解決→計画(`logical_plan`)→実行という4段階になる。第19章では、
//! `LogicalPlan`から実行アルゴリズムを確定した`PhysicalPlan`を作り、
//! `Executor::next()`で1行ずつ引っ張り出すVolcano型のPull実行を行う
//! `physical_plan`が加わる。`database`の実行経路は構文解析→名前解決→
//! 論理計画→物理計画→実行という5段階になり、`EXPLAIN`で`PhysicalPlan`の
//! 木を確認できるようになる。第20章では、`CREATE TABLE`の列制約に`PRIMARY
//! KEY`・`UNIQUE`が加わり、その一意性を走査ベースで検査する`constraints`が
//! 加わる。`INSERT`・`UPDATE`は、対象行すべての検査を終えるまで書き込みを
//! 一切始めないStatement Rollbackの対象に、この一意性検査も含めるようになる。

pub mod ast;
pub mod binder;
pub mod buffer_pool;
pub mod catalog;
pub mod constraints;
pub mod database;
pub mod disk_manager;
pub mod error;
pub mod eval;
pub mod executor;
pub mod free_space_map;
pub mod heap_file;
pub mod ids;
pub mod lexer;
pub mod logical_plan;
pub mod page;
pub mod parser;
pub mod physical_plan;
pub mod slotted_page;
pub mod storage;
pub mod storage_mem;
pub mod tuple_codec;
pub mod types;

pub use ast::{Expr, Statement};
pub use binder::{Binder, BoundExpr, BoundStatement, CatalogLookup};
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
pub use logical_plan::LogicalPlan;
pub use page::{
    FILE_HEADER_SIZE, FORMAT_VERSION, MAGIC, PAGE_HEADER_SIZE, PAGE_PAYLOAD_SIZE, PAGE_SIZE,
    FileHeader, Page, PageType,
};
pub use parser::parse_statement;
pub use physical_plan::{Executor, PhysicalPlan};
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
