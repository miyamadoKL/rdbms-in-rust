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

/// Slotted Page(第12章)内の1スロットを指す識別子。
///
/// `PageId`とは異なり、この番号はページの外では意味を持たない。あるページの
/// スロット3と、別のページのスロット3は無関係な区画であり、`RecordId`として
/// `PageId`と組み合わせて初めてデータベース全体で1つのタプルを指し示せる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SlotId(pub u16);

/// Slotted Page上のタプル1件を一意に指す識別子(Record ID、RID)。
///
/// `PageId`だけでは同じページ内の複数のタプルを区別できず、`SlotId`だけでは
/// どのページのスロットを指しているのかが分からない。この2つの組がそろって
/// 初めて、データベース全体でタプル1件の位置を一意に表せる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecordId {
    /// このタプルが格納されているページ。
    pub page_id: PageId,
    /// そのページ内でのスロット番号。
    pub slot_id: SlotId,
}

impl RecordId {
    /// `page_id`のページの`slot_id`番スロットを指す`RecordId`を作る。
    pub fn new(page_id: PageId, slot_id: SlotId) -> Self {
        RecordId { page_id, slot_id }
    }
}
