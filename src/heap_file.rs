//! 複数の`Page`(第11章)をまとめて1つのテーブルとして扱うHeap File。
//!
//! `SlottedPage`(第12章)は1ページの中だけを扱い、`DiskManager`(この章の前半)は
//! 1ページを指定した番号で読み書きするだけで、どのページの集まりが1つのテーブルを
//! なすかを知らない。`HeapFile`が、その「ページの集まりとしてのテーブル」を表す。
//!
//! # 1つのファイルは1つのHeap File
//!
//! この章の`HeapFile`は、1つの`DiskManager`(1つのファイル)を丸ごと1個の
//! テーブルとして占有する。`HeapFile::open`は、ページ0(Metaページ)を除く
//! 全ページを、そのテーブルが持つデータページとみなして走査対象に加える。
//!
//! 複数のテーブルを1つのファイルに共存させ、各テーブルが自分の使うページ番号の
//! 一覧をどこかに永続化しておく仕組みは、この章にはまだない。それを担う
//! カタログとFree Space Mapは第15章で導入する。この章の時点で「プロセスを
//! 再起動してもテーブルのデータが残る」ことを確認するテストは、
//! `HeapFile::open`が毎回ファイル全体を走査してページ一覧を作り直すという
//! この章の設計にそのまま乗っている。
//!
//! # ページ探索は線形探索
//!
//! `insert`は、空きのあるページを`page_ids`の先頭から順に探す素朴な線形探索で
//! 実装する。テーブルのページ数が増えるほど、空きページを探すコストもページ数に
//! 比例して増えていく。どのページにどれだけ空きがあるかを別に記録しておき、
//! 空きのあるページを直接指せるようにする**Free Space Map**は第15章で扱う。
//!
//! # `update`でRecordIdが変わりうる
//!
//! `update`は、新しいバイト列が元のページに(コンパクション後も)収まる限り、
//! 同じ`RecordId`を保ったまま更新する。しかし、値が大きくなってページの外へ
//! はみ出す場合は、そのページからは削除し、空きのある別のページへ挿入し直す。
//! この移動によって`RecordId`は変わる。`SlottedPage::update`が1ページの中で
//! できる範囲の付け替えしか行わないのと同じ理由で、`HeapFile::update`も
//! ページをまたぐ移動までは1ページの仕組みの上に素直に積み上げただけであり、
//! `RecordId`を移動後も固定するための間接参照(たとえば「移動先を指すポインタを
//! 元の場所に残す」といった仕組み)は導入しない。呼び出し側は、`update`が返す
//! `RecordId`を以後のアクセスに使う必要がある。

use crate::disk_manager::DiskManager;
use crate::error::{DbError, DbResult};
use crate::ids::{PageId, RecordId, SlotId};
use crate::page::{Page, PageType};
use crate::slotted_page::{SlotStatus, SlottedPage};

/// 複数ページにまたがる1つのテーブルを表す。
pub struct HeapFile {
    disk: DiskManager,
    /// このテーブルが使っているデータページの一覧(挿入順ではなく、
    /// ファイル中のページ番号順)。
    page_ids: Vec<PageId>,
}

impl HeapFile {
    /// `disk`が管理するファイル全体を1つのHeap Fileとして開く。
    ///
    /// ページ0(Meta)を除く全ページをデータページとみなし、`page_ids`に登録する。
    pub fn open(disk: DiskManager) -> Self {
        let page_ids = (1..disk.page_count()).map(PageId).collect();
        HeapFile { disk, page_ids }
    }

    /// このテーブルが使っているデータページの一覧。
    pub fn page_ids(&self) -> &[PageId] {
        &self.page_ids
    }

    /// `bytes`を新しいタプルとして挿入し、それを指す`RecordId`を返す。
    ///
    /// 既存のページを先頭から順に試し、`SlottedPage::insert`が入る場所を
    /// 見つけられた最初のページへ書き込む。どのページにも入らなければ、
    /// 新しいページを1枚割り当ててそこへ書き込む。新しいページに`SlottedPage::init`
    /// した直後ですら`bytes`が入らない場合は、`bytes`がページの`payload`に対して
    /// 大きすぎるということなので`DbError::TupleTooLarge`を返す。
    pub fn insert(&mut self, bytes: &[u8]) -> DbResult<RecordId> {
        for &page_id in &self.page_ids {
            let mut page = self.disk.read_page(page_id)?;
            if let Some(slot) = SlottedPage::open(page.payload_mut()).insert(bytes) {
                self.disk.write_page(&page)?;
                return Ok(RecordId::new(page_id, slot));
            }
        }

        let page_id = self.disk.allocate_page(PageType::Data)?;
        let mut page = self.disk.read_page(page_id)?;
        let slot = SlottedPage::init(page.payload_mut())
            .insert(bytes)
            .ok_or(DbError::TupleTooLarge(bytes.len()))?;
        self.disk.write_page(&page)?;
        self.page_ids.push(page_id);
        Ok(RecordId::new(page_id, slot))
    }

    /// `rid`が指すタプルのバイト列を返す。削除済み、またはそもそも挿入されて
    /// いなければ`None`を返す。
    pub fn get(&self, rid: RecordId) -> DbResult<Option<Vec<u8>>> {
        let mut page = self.disk.read_page(rid.page_id)?;
        Ok(SlottedPage::open(page.payload_mut())
            .get(rid.slot_id)
            .map(|bytes| bytes.to_vec()))
    }

    /// `rid`が指すタプルを削除する。削除できたら`true`、対象がすでに存在しない
    /// (未挿入、または削除済み)なら`false`を返す。
    pub fn delete(&mut self, rid: RecordId) -> DbResult<bool> {
        let mut page = self.disk.read_page(rid.page_id)?;
        let deleted = SlottedPage::open(page.payload_mut()).delete(rid.slot_id);
        if deleted {
            self.disk.write_page(&page)?;
        }
        Ok(deleted)
    }

    /// `rid`が指すタプルを`bytes`へ置き換える。
    ///
    /// 対象が存在しなければ`Ok(None)`を返す。存在すれば、更新後のタプルを指す
    /// `RecordId`を`Ok(Some(rid))`で返す。この`RecordId`は、ページ内で更新できた
    /// 場合は引数の`rid`と同じだが、ページをまたぐ移動が起きた場合は新しい値になる
    /// (モジュールの説明を参照)。
    pub fn update(&mut self, rid: RecordId, bytes: &[u8]) -> DbResult<Option<RecordId>> {
        let mut page = self.disk.read_page(rid.page_id)?;

        let occupied =
            SlottedPage::open(page.payload_mut()).status(rid.slot_id) == Some(SlotStatus::Occupied);
        if !occupied {
            return Ok(None);
        }

        if SlottedPage::open(page.payload_mut()).update(rid.slot_id, bytes) {
            self.disk.write_page(&page)?;
            return Ok(Some(rid));
        }

        // このページの中には(コンパクションしても)収まらないので、
        // このページからは削除し、別のページへ挿入し直す。
        SlottedPage::open(page.payload_mut()).delete(rid.slot_id);
        self.disk.write_page(&page)?;
        let new_rid = self.insert(bytes)?;
        Ok(Some(new_rid))
    }

    /// 全ページを先頭から順に走査し、生きている(削除されていない)全タプルを
    /// `(RecordId, タプルのバイト列)`として返すイテレータ。
    pub fn scan(&self) -> Scan<'_> {
        Scan {
            disk: &self.disk,
            page_ids: self.page_ids.iter(),
            current: None,
        }
    }
}

/// [`HeapFile::scan`]が返すイテレータ。
///
/// 現在読み込み中のページとその中の走査位置(`slot_idx`)だけを保持し、ページ内の
/// 全スロットを見終えたら次のページを`DiskManager`から読み込む。この章の実装は
/// ページ単位でしかバッファリングせず、`HeapFile`全体の内容を一度にメモリへ
/// 読み込むことはしない。
pub struct Scan<'a> {
    disk: &'a DiskManager,
    page_ids: std::slice::Iter<'a, PageId>,
    current: Option<(Page, u16)>,
}

impl Iterator for Scan<'_> {
    type Item = DbResult<(RecordId, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((page, slot_idx)) = self.current.as_mut() {
                let page_id = page.page_id;
                let slot_count = SlottedPage::open(page.payload_mut()).slot_count() as u16;
                while *slot_idx < slot_count {
                    let slot = SlotId(*slot_idx);
                    *slot_idx += 1;
                    if let Some(bytes) = SlottedPage::open(page.payload_mut()).get(slot) {
                        let rid = RecordId::new(page_id, slot);
                        return Some(Ok((rid, bytes.to_vec())));
                    }
                }
                self.current = None;
                continue;
            }

            let next_page_id = *self.page_ids.next()?;
            match self.disk.read_page(next_page_id) {
                Ok(page) => self.current = Some((page, 0)),
                Err(err) => return Some(Err(err)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-heap-file-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        path
    }

    #[test]
    fn insert_then_get_round_trips() {
        let path = temp_path("insert-get");
        let disk = DiskManager::open(&path).unwrap();
        let mut heap = HeapFile::open(disk);

        let rid = heap.insert(b"alice").unwrap();
        assert_eq!(heap.get(rid).unwrap(), Some(b"alice".to_vec()));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_hides_the_tuple_from_get_and_scan() {
        let path = temp_path("delete");
        let disk = DiskManager::open(&path).unwrap();
        let mut heap = HeapFile::open(disk);

        let rid = heap.insert(b"gone soon").unwrap();
        assert!(heap.delete(rid).unwrap());
        assert_eq!(heap.get(rid).unwrap(), None);
        assert_eq!(heap.scan().count(), 0);
        // 存在しないRecordIdの再削除はfalseを返す。
        assert!(!heap.delete(rid).unwrap());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn update_in_place_keeps_the_record_id() {
        let path = temp_path("update-in-place");
        let disk = DiskManager::open(&path).unwrap();
        let mut heap = HeapFile::open(disk);

        let rid = heap.insert(b"aaaaa").unwrap();
        let new_rid = heap.update(rid, b"bbbbb").unwrap().unwrap();

        assert_eq!(new_rid, rid);
        assert_eq!(heap.get(rid).unwrap(), Some(b"bbbbb".to_vec()));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn update_that_does_not_fit_moves_to_another_page_and_changes_the_record_id() {
        let path = temp_path("update-move");
        let disk = DiskManager::open(&path).unwrap();
        let mut heap = HeapFile::open(disk);

        // 1ページ目に2件を隙間なく詰める。どちらも生きているので、
        // 片方を削除してコンパクションしても、もう片方の分だけ空きは
        // 埋まったままになる。
        let a = vec![b'a'; 2000];
        let b = vec![b'b'; 2000];
        let _rid_a = heap.insert(&a).unwrap();
        let rid_b = heap.insert(&b).unwrap();

        // aが生きたまま残るページには、この大きさは(コンパクションしても)
        // 収まらないため、別ページへ移動するはずである。
        let bigger = vec![b'c'; 3000];
        let new_rid = heap.update(rid_b, &bigger).unwrap().unwrap();

        assert_ne!(new_rid.page_id, rid_b.page_id);
        assert_eq!(heap.get(rid_b).unwrap(), None);
        assert_eq!(heap.get(new_rid).unwrap(), Some(bigger));
        assert_eq!(heap.get(_rid_a).unwrap(), Some(a));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn update_on_missing_record_returns_none() {
        let path = temp_path("update-missing");
        let disk = DiskManager::open(&path).unwrap();
        let mut heap = HeapFile::open(disk);

        let rid = heap.insert(b"x").unwrap();
        heap.delete(rid).unwrap();
        assert_eq!(heap.update(rid, b"y").unwrap(), None);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn insert_across_multiple_pages_and_scan_returns_them_all() {
        let path = temp_path("multi-page-scan");
        let disk = DiskManager::open(&path).unwrap();
        let mut heap = HeapFile::open(disk);

        // 1ページに収まらない件数を入れ、複数ページへまたがらせる。
        let mut inserted = Vec::new();
        for i in 0..500u32 {
            let bytes = format!("row-{i:04}").into_bytes();
            let rid = heap.insert(&bytes).unwrap();
            inserted.push((rid, bytes));
        }

        assert!(heap.page_ids().len() > 1);

        let scanned: Vec<_> = heap.scan().collect::<DbResult<Vec<_>>>().unwrap();
        assert_eq!(scanned.len(), inserted.len());
        for (rid, bytes) in &inserted {
            assert!(scanned.contains(&(*rid, bytes.clone())));
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopening_the_disk_manager_preserves_the_heap_file_contents() {
        let path = temp_path("reopen");
        let mut inserted = Vec::new();
        {
            let disk = DiskManager::open(&path).unwrap();
            let mut heap = HeapFile::open(disk);
            for i in 0..300u32 {
                let bytes = format!("row-{i:04}").into_bytes();
                let rid = heap.insert(&bytes).unwrap();
                inserted.push((rid, bytes));
            }
            // heapのDiskManagerはここでスコープを抜けてdropされる(closeに相当)。
        }

        let disk = DiskManager::open(&path).unwrap();
        let heap = HeapFile::open(disk);
        let scanned: Vec<_> = heap.scan().collect::<DbResult<Vec<_>>>().unwrap();
        assert_eq!(scanned.len(), inserted.len());
        for (rid, bytes) in &inserted {
            assert_eq!(heap.get(*rid).unwrap(), Some(bytes.clone()));
        }

        std::fs::remove_file(&path).unwrap();
    }
}
