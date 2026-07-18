//! ドメイン上意味の異なる `u64` を取り違えないための Newtype 群。
//!
//! 後続の章(ストレージ、トランザクション)で使われる識別子の骨格。

/// ディスク上の1ページを指す識別子。
///
/// `TableId` とは型が異なるため、呼び出し側が引数を取り違えてもコンパイルエラーになる。
///
/// ```compile_fail
/// use minidb::{PageId, TableId};
///
/// fn load_page(page_id: PageId) {}
///
/// let table_id = TableId(1);
/// load_page(table_id); // 型が違うためコンパイルエラー
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageId(pub u64);

/// カタログに登録されたテーブルを指す識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableId(pub u64);

/// トランザクションを指す識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransactionId(pub u64);
