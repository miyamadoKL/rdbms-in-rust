//! 複数の`Page`(第11章)をまとめて1つのテーブルとして扱うHeap File。
//!
//! `SlottedPage`(第12章)は1ページの中だけを扱い、`DiskManager`(第13章)は
//! 1ページを指定した番号で読み書きするだけで、どのページの集まりが1つのテーブルを
//! なすかを知らない。`HeapFile`が、その「ページの集まりとしてのテーブル」を表す。
//!
//! # ページへのアクセスは`BufferPool`経由
//!
//! 第13章の`HeapFile`は`DiskManager`を直接叩いていたが、この章では
//! `BufferPool`(第14章)を経由する。`disk.read_page(id)? -> Page`のように
//! ページを値として受け取ってから`disk.write_page(&page)`で書き戻す代わりに、
//! `pool.write_page(id)? -> PageWriteGuard`が返す`&mut [u8]`を直接書き換え、
//! 書き戻しはGuardの`Drop`に任せる。この置き換えにともない、`insert`・
//! `update`・`delete`の内部から明示的な`write_page`呼び出しが消えている。
//! 呼び出し側に見える公開シグネチャ(`open`が`DiskManager`ではなく
//! `BufferPool`を受け取る点を除く)は変えていない。
//!
//! 読み取りだけで済む`get`・`scan`は`pool.read_page(id)? -> PageReadGuard`を
//! 使う。`PageReadGuard::data()`が返すのは`&[u8]`(不変参照)で、`SlottedPage`
//! (第12章)を開くための`&mut [u8]`は要求しない。代わりに、読み取り専用の
//! `SlottedPageRef`(第14章で追加)を`SlottedPageRef::open(guard.data())`の
//! 形で使う。
//!
//! # 1つのファイルは1つのHeap File
//!
//! この章の`HeapFile`は、1つの`BufferPool`(1つのファイル)を丸ごと1個の
//! テーブルとして占有する。`HeapFile::open`は、ページ0(Metaページ)を除く
//! 全ページを、そのテーブルが持つデータページとみなして走査対象に加える。
//!
//! 複数のテーブルを1つのファイルに共存させ、各テーブルが自分の使うページ番号の
//! 一覧をどこかに永続化しておく仕組みは、この章にはまだない。それを担う
//! カタログとFree Space Mapは第15章で導入する。この章の時点で「プロセスを
//! 再起動してもテーブルのデータが残る」ことを確認するテストは、
//! `HeapFile::open`が毎回ファイル全体を走査してページ一覧を作り直すという
//! この章の設計にそのまま乗っている。ただし、`BufferPool`はdirtyなページを
//! 明示的に`flush`するまでディスクへ書き戻さないため、再起動を確認するテストは
//! `HeapFile::flush`を呼んでからファイルを閉じる必要がある(モジュール末尾の
//! テストを参照)。
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

use crate::buffer_pool::BufferPool;
use crate::error::{DbError, DbResult};
use crate::ids::{PageId, RecordId, SlotId};
use crate::page::{PAGE_PAYLOAD_SIZE, PageType};
use crate::slotted_page::{SlotStatus, SlottedPage, SlottedPageRef, max_len_for_fresh_page};

/// 複数ページにまたがる1つのテーブルを表す。
pub struct HeapFile {
    pool: BufferPool,
    /// このテーブルが使っているデータページの一覧(挿入順ではなく、
    /// ファイル中のページ番号順)。
    page_ids: Vec<PageId>,
}

impl HeapFile {
    /// `pool`が管理するファイル全体を1つのHeap Fileとして開く。
    ///
    /// ページ0(Meta)を除く全ページをデータページとみなし、`page_ids`に登録する。
    pub fn open(pool: BufferPool) -> Self {
        let page_ids = (1..pool.page_count()).map(PageId).collect();
        HeapFile { pool, page_ids }
    }

    /// このテーブルが使っているデータページの一覧。
    pub fn page_ids(&self) -> &[PageId] {
        &self.page_ids
    }

    /// このテーブルの`BufferPool`にキャッシュされているdirtyなページを
    /// すべてディスクへ書き戻す。
    ///
    /// `BufferPool::flush_all`をそのまま呼ぶだけの薄いラッパーで、実ディスクへの
    /// 同期(`DiskManager::sync`)までは行わない。`HeapFile`は`BufferPool`を
    /// privateフィールドとして所有しており、呼び出し側が`DiskManager`へ
    /// 直接触れる経路はこの章にはまだない。実ディスクへの同期まで呼び出し側が
    /// 明示的に行えるようにする層は第15章の`Storage::sync`で追加する。
    pub fn flush(&self) -> DbResult<()> {
        self.pool.flush_all()
    }

    /// `bytes`を新しいタプルとして挿入し、それを指す`RecordId`を返す。
    ///
    /// 既存のページを先頭から順に試し、`SlottedPage::insert`が入る場所を
    /// 見つけられた最初のページへ書き込む。どのページにも入らなければ、
    /// 新しいページを1枚割り当ててそこへ書き込む。
    ///
    /// `bytes`が空の1ページにも収まらないほど大きい(`max_len_for_fresh_page`
    /// 参照)場合は、どのページも読み書きせず、新しいページも確保せずに
    /// `DbError::TupleTooLarge`を返す。この事前検査が無いと、失敗するだけの
    /// `insert`のたびに`allocate_page`でファイルを1ページ伸ばしてしまい、
    /// 同じ大きすぎる値を何度も`insert`しようとするコードがファイルサイズを
    /// 際限なく肥大化させる。
    pub fn insert(&mut self, bytes: &[u8]) -> DbResult<RecordId> {
        if bytes.len() > max_len_for_fresh_page(PAGE_PAYLOAD_SIZE) {
            return Err(DbError::TupleTooLarge(bytes.len()));
        }

        for &page_id in &self.page_ids {
            let mut guard = self.pool.write_page(page_id)?;
            if let Some(slot) = SlottedPage::open(guard.data_mut())?.insert(bytes) {
                return Ok(RecordId::new(page_id, slot));
            }
        }

        let page_id = self.pool.allocate_page(PageType::Data)?;
        let mut guard = self.pool.write_page(page_id)?;
        let slot = SlottedPage::init(guard.data_mut())
            .insert(bytes)
            .ok_or(DbError::TupleTooLarge(bytes.len()))?;
        drop(guard);
        self.page_ids.push(page_id);
        Ok(RecordId::new(page_id, slot))
    }

    /// `rid`が指すタプルのバイト列を返す。削除済み、またはそもそも挿入されて
    /// いなければ`None`を返す。
    pub fn get(&self, rid: RecordId) -> DbResult<Option<Vec<u8>>> {
        let guard = self.pool.read_page(rid.page_id)?;
        Ok(SlottedPageRef::open(guard.data())?
            .get(rid.slot_id)
            .map(|bytes| bytes.to_vec()))
    }

    /// `rid`が指すタプルを削除する。削除できたら`true`、対象がすでに存在しない
    /// (未挿入、または削除済み)なら`false`を返す。
    pub fn delete(&mut self, rid: RecordId) -> DbResult<bool> {
        let mut guard = self.pool.write_page(rid.page_id)?;
        Ok(SlottedPage::open(guard.data_mut())?.delete(rid.slot_id))
    }

    /// `rid`が指すタプルを`bytes`へ置き換える。
    ///
    /// 対象が存在しなければ`Ok(None)`を返す。存在すれば、更新後のタプルを指す
    /// `RecordId`を`Ok(Some(rid))`で返す。この`RecordId`は、ページ内で更新できた
    /// 場合は引数の`rid`と同じだが、ページをまたぐ移動が起きた場合は新しい値になる
    /// (モジュールの説明を参照)。
    ///
    /// # ページをまたぐ移動は「挿入してから削除する」
    ///
    /// 元のページに(コンパクションしても)収まらない場合、新しい場所へ`insert`
    /// してから、それが成功したときに限って元の行を`delete`する。逆の順序
    /// (先に削除してから挿入する)を選ぶと、挿入が`DbError::TupleTooLarge`などで
    /// 失敗したときに元の行がすでに消えてしまい、`UPDATE`の失敗が行の消失に
    /// つながる。`SlottedPage::update`自身は収まらないときに対象を書き換えずに
    /// `false`を返す(該当箇所のドキュメントを参照)ため、ページ内更新の失敗では
    /// この問題は起きない。問題が起きうるのはページをまたぐ移動のときだけである。
    ///
    /// `bytes`が空の1ページにも収まらないほど大きい場合は、`insert`と同じく
    /// どのページも変更せずに`DbError::TupleTooLarge`を返す(`insert`の
    /// ドキュメントを参照)。
    pub fn update(&mut self, rid: RecordId, bytes: &[u8]) -> DbResult<Option<RecordId>> {
        // 対象が存在するかどうかは読み取り専用のGuardで確かめる。存在しない
        // 場合にまで`write_page`でpinしてdirty扱いにしてしまうと、evict時の
        // 無駄な書き戻しが増える。
        let occupied = {
            let guard = self.pool.read_page(rid.page_id)?;
            SlottedPageRef::open(guard.data())?.status(rid.slot_id) == Some(SlotStatus::Occupied)
        };
        if !occupied {
            return Ok(None);
        }

        if bytes.len() > max_len_for_fresh_page(PAGE_PAYLOAD_SIZE) {
            return Err(DbError::TupleTooLarge(bytes.len()));
        }

        {
            let mut guard = self.pool.write_page(rid.page_id)?;
            if SlottedPage::open(guard.data_mut())?.update(rid.slot_id, bytes) {
                return Ok(Some(rid));
            }
            // このページの中には(コンパクションしても)収まらない。ここでは
            // まだ元の行を削除しない(モジュールのドキュメントを参照)。
        }

        let new_rid = self.insert(bytes)?;
        match self.delete(rid) {
            Ok(true) => Ok(Some(new_rid)),
            Ok(false) => {
                // 直前にoccupiedを確認済みで、この章はシングルスレッド前提
                // なので通常は起こらない。万一起きた場合は、すでに書き込んだ
                // 新しい行をロールバックしてから異常として報告する。
                let _ = self.delete(new_rid);
                Err(DbError::CorruptPage(format!(
                    "update: 元のRecordId({rid:?})の削除に失敗しました(想定外)"
                )))
            }
            Err(err) => {
                let _ = self.delete(new_rid);
                Err(err)
            }
        }
    }

    /// 全ページを先頭から順に走査し、生きている(削除されていない)全タプルを
    /// `(RecordId, タプルのバイト列)`として返すイテレータ。
    pub fn scan(&self) -> Scan<'_> {
        Scan::new(&self.pool, &self.page_ids)
    }
}

/// [`HeapFile::scan`]が返すイテレータ。
///
/// 現在読み込み中のページとその中の走査位置(`slot_idx`)だけを保持し、ページ内の
/// 全スロットを見終えたら次のページを`BufferPool`から読み込む。この章の実装は
/// ページ単位でしかバッファリングせず、`HeapFile`全体の内容を一度にメモリへ
/// 読み込むことはしない。
pub struct Scan<'a> {
    pool: &'a BufferPool,
    page_ids: std::slice::Iter<'a, PageId>,
    current: Option<(crate::buffer_pool::PageReadGuard<'a>, u16)>,
}

impl<'a> Scan<'a> {
    /// `pool`と`page_ids`を指定して走査を組み立てる。
    ///
    /// `HeapFile::scan`が使う入口だが、`pool`は`&BufferPool`、`page_ids`は
    /// `&[PageId]`という2つの独立した参照だけを要求するため、両方を1つの構造体に
    /// まとめて所有している`HeapFile`以外からも呼べる。第15章の`Storage`は、
    /// 複数のテーブルを1つの`BufferPool`の上で管理し、テーブルごとの`page_ids`を
    /// 別の場所(カタログ)に持つため、`HeapFile`そのものは使わずこの構築子だけを
    /// 再利用する。
    pub(crate) fn new(pool: &'a BufferPool, page_ids: &'a [PageId]) -> Self {
        Scan {
            pool,
            page_ids: page_ids.iter(),
            current: None,
        }
    }
}

impl Iterator for Scan<'_> {
    type Item = DbResult<(RecordId, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((guard, slot_idx)) = self.current.as_mut() {
                let page_id = guard.page_id();
                let view = match SlottedPageRef::open(guard.data()) {
                    Ok(view) => view,
                    Err(err) => return Some(Err(err)),
                };
                let slot_count = view.slot_count() as u16;
                while *slot_idx < slot_count {
                    let slot = SlotId(*slot_idx);
                    *slot_idx += 1;
                    if let Some(bytes) = view.get(slot) {
                        let rid = RecordId::new(page_id, slot);
                        return Some(Ok((rid, bytes.to_vec())));
                    }
                }
                self.current = None;
                continue;
            }

            let next_page_id = *self.page_ids.next()?;
            match self.pool.read_page(next_page_id) {
                Ok(guard) => self.current = Some((guard, 0)),
                Err(err) => return Some(Err(err)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_manager::DiskManager;

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

    /// 容量16フレームのBufferPoolでHeap Fileを開く。テストで十分に余裕のある
    /// 容量にしておき、Buffer Pool自体のeviction挙動は`buffer_pool`モジュール
    /// 側のテストで確認する。
    fn open_heap(path: &std::path::Path) -> HeapFile {
        let disk = DiskManager::open(path).unwrap();
        HeapFile::open(BufferPool::new(disk, 16))
    }

    #[test]
    fn insert_then_get_round_trips() {
        let path = temp_path("insert-get");
        let mut heap = open_heap(&path);

        let rid = heap.insert(b"alice").unwrap();
        assert_eq!(heap.get(rid).unwrap(), Some(b"alice".to_vec()));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_hides_the_tuple_from_get_and_scan() {
        let path = temp_path("delete");
        let mut heap = open_heap(&path);

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
        let mut heap = open_heap(&path);

        let rid = heap.insert(b"aaaaa").unwrap();
        let new_rid = heap.update(rid, b"bbbbb").unwrap().unwrap();

        assert_eq!(new_rid, rid);
        assert_eq!(heap.get(rid).unwrap(), Some(b"bbbbb".to_vec()));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn update_that_does_not_fit_moves_to_another_page_and_changes_the_record_id() {
        let path = temp_path("update-move");
        let mut heap = open_heap(&path);

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
    fn update_that_fails_to_insert_leaves_the_original_row_intact() {
        let path = temp_path("update-insert-fails");
        let mut heap = open_heap(&path);

        let rid = heap.insert(b"original").unwrap();
        heap.flush().unwrap();
        let page_ids_before = heap.page_ids().to_vec();
        let size_before = std::fs::metadata(&path).unwrap().len();

        // 空の1ページにも収まらないほど大きい値へのupdateは、まず新しい場所への
        // insertを試み、それがTupleTooLargeで失敗する。旧行を先に消していれば
        // この時点でデータが失われるが、insertを先に試す実装ではrid経由の
        // 元の行がそのまま読める。
        let too_big = vec![b'x'; crate::page::PAGE_PAYLOAD_SIZE + 1];
        let err = heap.update(rid, &too_big).unwrap_err();
        assert!(matches!(err, DbError::TupleTooLarge(_)));
        heap.flush().unwrap();

        assert_eq!(heap.get(rid).unwrap(), Some(b"original".to_vec()));
        // 失敗したupdateは、insert前の事前検査で弾かれるべきであり、新しい
        // ページを確保してはならない(ページ数・ファイルサイズが変わらない)。
        assert_eq!(heap.page_ids(), page_ids_before.as_slice());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size_before);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn insert_that_is_too_large_does_not_grow_the_file() {
        let path = temp_path("insert-too-large-no-growth");
        let mut heap = open_heap(&path);

        let rid = heap.insert(b"small").unwrap();
        heap.flush().unwrap();
        let page_ids_before = heap.page_ids().to_vec();
        let size_before = std::fs::metadata(&path).unwrap().len();

        // 空の1ページにも収まらないほど大きいinsertを繰り返しても、
        // 新しいページを確保してはならない。事前検査が無いと、失敗する
        // insertのたびにallocate_pageでファイルが1ページずつ伸びてしまう。
        let too_big = vec![b'x'; crate::page::PAGE_PAYLOAD_SIZE + 1];
        for _ in 0..3 {
            let err = heap.insert(&too_big).unwrap_err();
            assert!(matches!(err, DbError::TupleTooLarge(_)));
        }
        heap.flush().unwrap();

        assert_eq!(heap.page_ids(), page_ids_before.as_slice());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size_before);
        assert_eq!(heap.get(rid).unwrap(), Some(b"small".to_vec()));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn update_on_missing_record_returns_none() {
        let path = temp_path("update-missing");
        let mut heap = open_heap(&path);

        let rid = heap.insert(b"x").unwrap();
        heap.delete(rid).unwrap();
        assert_eq!(heap.update(rid, b"y").unwrap(), None);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn insert_across_multiple_pages_and_scan_returns_them_all() {
        let path = temp_path("multi-page-scan");
        let mut heap = open_heap(&path);

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
            // page_ids::pushで容量を使い切らないよう、十分な容量を確保する。
            let mut heap = HeapFile::open(BufferPool::new(disk, 8));
            for i in 0..300u32 {
                let bytes = format!("row-{i:04}").into_bytes();
                let rid = heap.insert(&bytes).unwrap();
                inserted.push((rid, bytes));
            }
            // BufferPoolはdirtyなページを明示的にflushするまで書き戻さない。
            // 第13章のDiskManager::syncと同様、書き戻し自体はheap(と、その中の
            // BufferPool)がスコープを抜けてdropされる前に呼んでおく必要がある。
            heap.flush().unwrap();
            // heapはここでスコープを抜けてdropされる(closeに相当)。
        }

        let disk = DiskManager::open(&path).unwrap();
        let heap = HeapFile::open(BufferPool::new(disk, 8));
        let scanned: Vec<_> = heap.scan().collect::<DbResult<Vec<_>>>().unwrap();
        assert_eq!(scanned.len(), inserted.len());
        for (rid, bytes) in &inserted {
            assert_eq!(heap.get(*rid).unwrap(), Some(bytes.clone()));
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn heap_file_works_with_a_buffer_pool_smaller_than_the_page_count() {
        // BufferPoolの容量がテーブルのページ数より小さくても、eviction
        // 経由で正しく動作することを確認する(HeapFile自体はBufferPoolの
        // 容量を意識しない)。
        let path = temp_path("small-pool");
        let disk = DiskManager::open(&path).unwrap();
        let mut heap = HeapFile::open(BufferPool::new(disk, 2));

        let mut inserted = Vec::new();
        for i in 0..500u32 {
            let bytes = format!("row-{i:04}").into_bytes();
            let rid = heap.insert(&bytes).unwrap();
            inserted.push((rid, bytes));
        }
        assert!(heap.page_ids().len() > 1);

        for (rid, bytes) in &inserted {
            assert_eq!(heap.get(*rid).unwrap(), Some(bytes.clone()));
        }

        std::fs::remove_file(&path).unwrap();
    }
}
