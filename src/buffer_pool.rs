//! `DiskManager`(第13章)の上に置く固定容量のページキャッシュ。
//!
//! `DiskManager`は`read_page`を呼ぶたびに必ず実ファイルへ`seek`・`read`する。
//! 同じページを1000回参照すれば、`DiskManager::io_count`も1000だけ増える。
//! `BufferPool`は、よく使うページのバイト列をメモリ上の**フレーム**に留めておき、
//! 2回目以降の参照をディスクI/Oなしで済ませる。
//!
//! # ページへのアクセスはGuard経由に限る
//!
//! `read_page`・`write_page`は、ページの中身へ直接触れる`&[u8]`・`&mut [u8]`を
//! 返すのではなく、[`PageReadGuard`]・[`PageWriteGuard`]という**RAII Guard**を返す。
//! ページを参照している間、そのページは**pin**されていて(このモジュールの
//! `FrameMeta::pin_count`)、Buffer Poolは決してpin中のページをevictしない。
//! Guardが`Drop`されるとpinが1つ外れ、`PageWriteGuard`の場合はさらにdirty flag
//! が立つ。呼び出し側がpinを外し忘れる可能性は、`unpin`という関数を呼び忘れる
//! というプログラマの規律の問題ではなく、Guardをスコープから出す(変数を
//! drop・シャドーイングする)というRustの通常の変数のライフタイムの問題に
//! 変わる。
//!
//! # Clock置換とpin
//!
//! 空きフレームがなく新しいページを読み込む必要があるとき、`BufferPool`は
//! **Clock置換**でevictするフレームを選ぶ。各フレームは参照ビット
//! (`referenced`)を持ち、フレームが参照されるたびに立てる。置換の候補を
//! 探す針(`clock_hand`)は全フレームを巡回し、参照ビットが立っているフレームは
//! ビットを倒して素通りし(もう一度巡ってきたときに初めてevict対象になる)、
//! pin中のフレームは常に候補から除外する。全フレームがpin中でevictできる
//! フレームが1つもなければ、`DbError::BufferPoolFull`を返す。
//!
//! # メタデータとページ本体を別の`Mutex`で守る
//!
//! フレームの`pin_count`・`dirty`・`referenced`・「今どのページを保持しているか」
//! は、フレームの配列とは別の`Inner`に切り出し、1本の`Mutex<Inner>`で守っている。
//! ページ本体(`Page`)は、フレームごとに独立した`Mutex<Frame>`で守る。
//!
//! この分離には理由がある。空きフレームを探すときも、Clock置換で候補を
//! 走査するときも、見なければならないのは`pin_count`のような小さな
//! メタデータだけであり、ページ本体そのものではない。もしメタデータを
//! フレーム本体と同じ`Mutex<Frame>`に同居させていたら、[`PageReadGuard`]が
//! 1枚のページをpinしている間ずっとその`Mutex`を握り続ける設計と衝突する。
//! 空きフレーム探しやClock置換の走査が、pin中で押さえられているフレームの
//! `Mutex`まで一度ロックしようとしてしまい、同じスレッドが同じ`Mutex`を
//! 二重にロックしようとして永久に止まる(自己デッドロック)。`pin_count`を
//! 別の軽い`Mutex<Inner>`に出しておけば、走査はページ本体に一切触れずに
//! 済み、この事故が起きない。
//!
//! # 単一スレッド前提の内部可変性
//!
//! この章の`minidb`はまだシングルスレッドで動いている(並行アクセスは第35章の
//! Latchまで登場しない)。それでも`&mut self`ではなく`&self`で読み書きできる
//! ようにしているのは、第13章の`DiskManager`が`&self`を選んだ理由と同じで、
//! `BufferPool`を`Arc`で複数の実行主体から共有できるようにしておくためである。
//! フレームごとに別々の`Mutex`を持つ構成は、のちに複数スレッドが同時に
//! 別々のページを読み書きできるようにするための布石でもある。ただし、
//! `page_table`の更新とフレームへの書き込みを1つの操作として原子的に行う
//! 保証はこの章にはまだなく、真の並行アクセスに対する安全性は第35章の
//! Latchで扱う。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::disk_manager::DiskManager;
use crate::error::{DbError, DbResult};
use crate::ids::PageId;
use crate::page::{Page, PageType};

/// 1フレームが保持するページ本体。`None`は「まだどのページも読み込んでいない
/// 空きフレーム」を表す。
struct Frame {
    page: Option<Page>,
}

/// 1フレームぶんのメタデータ。
struct FrameMeta {
    /// このフレームが今保持しているページ。`None`なら空きフレーム。
    occupant: Option<PageId>,
    /// このページを参照しているGuardの数。0より大きい間はevictされない。
    pin_count: u32,
    /// `PageWriteGuard`経由で変更された(可能性がある)ことを示すフラグ。
    /// evict時、このフラグが立っているページだけを`DiskManager::write_page`
    /// で書き戻す。
    dirty: bool,
    /// Clock置換の参照ビット。
    referenced: bool,
}

impl FrameMeta {
    fn empty() -> Self {
        FrameMeta {
            occupant: None,
            pin_count: 0,
            dirty: false,
            referenced: false,
        }
    }
}

/// `page_table`・全フレームのメタデータ・Clockの針・ヒット/ミス統計をまとめて
/// 保持する。フレーム本体(`Frame`)とは別の`Mutex`で守る(モジュール冒頭の
/// 説明を参照)。
struct Inner {
    /// どのページがどのフレーム番号に読み込まれているか。
    page_table: HashMap<PageId, usize>,
    meta: Vec<FrameMeta>,
    /// 次にClock置換の候補として調べるフレーム番号。
    clock_hand: usize,
    hits: u64,
    misses: u64,
}

/// `read_page`・`write_page`のヒット/ミス回数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferPoolStats {
    /// `page_table`にすでにページが載っていて、ディスクI/Oなしで返せた回数。
    pub hits: u64,
    /// `page_table`にページがなく、`DiskManager`から読み込んだ回数。
    pub misses: u64,
}

/// `DiskManager`の上に置く固定容量のページキャッシュ。
pub struct BufferPool {
    disk: DiskManager,
    /// フレームの配列。`new`で決めた容量のまま、以後は要素数を変えない。
    frames: Vec<Mutex<Frame>>,
    inner: Mutex<Inner>,
}

impl BufferPool {
    /// `disk`をキャッシュ元とし、`capacity`フレーム分のページを保持できる
    /// `BufferPool`を作る。
    ///
    /// `capacity`は0より大きい必要がある(0だとどのページも読み込めない)。
    pub fn new(disk: DiskManager, capacity: usize) -> Self {
        assert!(capacity > 0, "capacityは1以上である必要があります");
        let frames = (0..capacity).map(|_| Mutex::new(Frame { page: None })).collect();
        let meta = (0..capacity).map(|_| FrameMeta::empty()).collect();
        BufferPool {
            disk,
            frames,
            inner: Mutex::new(Inner {
                page_table: HashMap::new(),
                meta,
                clock_hand: 0,
                hits: 0,
                misses: 0,
            }),
        }
    }

    /// このBufferPoolが保持できるフレーム数。
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// このBufferPoolが管理する`DiskManager`の現在のページ数(Metaページを含む)。
    pub fn page_count(&self) -> u64 {
        self.disk.page_count()
    }

    /// 新しいページを`DiskManager`に1枚割り当てる。
    ///
    /// 割り当てた直後のページはまだこのBufferPoolに読み込まれていない。続けて
    /// `write_page`を呼ぶと、そのページを実際にディスクから読み込んでpinする
    /// (中身は全バイト0の初期状態)。
    pub fn allocate_page(&self, page_type: PageType) -> DbResult<PageId> {
        self.disk.allocate_page(page_type)
    }

    /// `id`のページを読み取り専用でpinし、[`PageReadGuard`]を返す。
    ///
    /// すでにキャッシュされていればヒットとしてディスクI/Oなしで返す。
    /// されていなければ`DiskManager::read_page`で読み込み、空きフレームが
    /// なければClock置換でフレームを1つ確保してから読み込む。
    pub fn read_page(&self, id: PageId) -> DbResult<PageReadGuard<'_>> {
        let frame_id = self.locate_and_pin(id)?;
        let guard = self.lock_frame(frame_id);
        Ok(PageReadGuard {
            pool: self,
            frame_id,
            page_id: id,
            guard,
        })
    }

    /// `id`のページを書き込み用にpinし、[`PageWriteGuard`]を返す。
    ///
    /// pinしている間の扱いは`read_page`と同じだが、返したGuardが`Drop`される
    /// ときにdirty flagを立てる点だけが異なる(モジュール冒頭の説明を参照)。
    pub fn write_page(&self, id: PageId) -> DbResult<PageWriteGuard<'_>> {
        let frame_id = self.locate_and_pin(id)?;
        let guard = self.lock_frame(frame_id);
        Ok(PageWriteGuard {
            pool: self,
            frame_id,
            page_id: id,
            guard,
        })
    }

    /// `id`のページがキャッシュされていて、かつdirtyなら`DiskManager`へ書き戻す。
    pub fn flush_page(&self, id: PageId) -> DbResult<()> {
        let frame_id = {
            let inner = self.lock_inner();
            match inner.page_table.get(&id) {
                Some(&frame_id) if inner.meta[frame_id].dirty => frame_id,
                _ => return Ok(()),
            }
        };
        self.flush_frame(frame_id)
    }

    /// キャッシュされている全ページのうち、dirtyなものをすべて`DiskManager`へ
    /// 書き戻す。
    ///
    /// 第13章の`DiskManager::sync`と同様、この呼び出し自体は`DiskManager`に
    /// 対する`sync`までは行わない。実際にディスクへ確実に届けるには、この後で
    /// `DiskManager::sync`を別途呼ぶ必要がある。
    pub fn flush_all(&self) -> DbResult<()> {
        let dirty_frames: Vec<usize> = {
            let inner = self.lock_inner();
            (0..self.frames.len())
                .filter(|&i| inner.meta[i].dirty)
                .collect()
        };
        for frame_id in dirty_frames {
            self.flush_frame(frame_id)?;
        }
        Ok(())
    }

    /// ヒット/ミス回数の累計。
    pub fn stats(&self) -> BufferPoolStats {
        let inner = self.lock_inner();
        BufferPoolStats {
            hits: inner.hits,
            misses: inner.misses,
        }
    }

    /// `frame_id`のページがdirtyなら書き戻し、dirty flagを下ろす。
    ///
    /// `Inner`のロックとフレームのロックを同時に持たない(モジュール冒頭の
    /// 説明を参照)。呼び出し側がすでにこのフレームをpinしているGuardを
    /// 保持したまま呼ぶと、フレームのロック待ちで止まるので注意すること。
    fn flush_frame(&self, frame_id: usize) -> DbResult<()> {
        let frame = self.lock_frame(frame_id);
        if let Some(page) = frame.page.as_ref() {
            self.disk.write_page(page)?;
        }
        drop(frame);
        let mut inner = self.lock_inner();
        inner.meta[frame_id].dirty = false;
        Ok(())
    }

    /// `id`のページを読み込み済みのフレーム番号を返し、そのフレームのpinを
    /// 1つ増やす(参照ビットも立てる)。
    fn locate_and_pin(&self, id: PageId) -> DbResult<usize> {
        let mut inner = self.lock_inner();
        let frame_id = self.locate_or_load(&mut inner, id)?;
        let meta = &mut inner.meta[frame_id];
        meta.pin_count += 1;
        meta.referenced = true;
        Ok(frame_id)
    }

    /// `id`のページが読み込まれているフレーム番号を返す。すでに`page_table`に
    /// あればヒット、なければミスとして`DiskManager`から読み込む(必要なら
    /// Clock置換でフレームを1つ空ける)。
    ///
    /// 空きフレーム探しとClock置換の走査は、`inner.meta`だけを見て判断し、
    /// フレーム本体の`Mutex`には一切触れない。フレーム本体をロックするのは、
    /// 空き(または今evictしたばかり)だと確定したフレームへ新しいページを
    /// 書き込む、この関数の最後の一歩だけである。
    fn locate_or_load(&self, inner: &mut Inner, id: PageId) -> DbResult<usize> {
        if let Some(&frame_id) = inner.page_table.get(&id) {
            inner.hits += 1;
            return Ok(frame_id);
        }
        inner.misses += 1;

        let frame_id = match inner.meta.iter().position(|m| m.occupant.is_none()) {
            Some(i) => i,
            None => self.evict(inner)?,
        };

        let page = self.disk.read_page(id)?;
        self.lock_frame(frame_id).page = Some(page);
        inner.meta[frame_id] = FrameMeta {
            occupant: Some(id),
            pin_count: 0,
            dirty: false,
            referenced: false,
        };
        inner.page_table.insert(id, frame_id);
        Ok(frame_id)
    }

    /// Clock置換でevictするフレームを1つ選び、空にしてその番号を返す。
    ///
    /// 針を最大`2 * capacity`ステップまで進める。1周目でpin中でない全フレームの
    /// 参照ビットを倒し、2周目で参照ビットが(倒された状態のまま)残っている
    /// フレームを見つける、という古典的なClockアルゴリズムの動作を、この
    /// 上限が保証する。この範囲でevict候補が見つからなければ、全フレームが
    /// pin中だということなので`DbError::BufferPoolFull`を返す。
    fn evict(&self, inner: &mut Inner) -> DbResult<usize> {
        let capacity = self.frames.len();
        for _ in 0..2 * capacity {
            let i = inner.clock_hand;
            inner.clock_hand = (inner.clock_hand + 1) % capacity;

            let meta = &mut inner.meta[i];
            if meta.occupant.is_none() || meta.pin_count > 0 {
                continue;
            }
            if meta.referenced {
                meta.referenced = false;
                continue;
            }

            let evicted_id = meta.occupant.take().expect("occupantはSomeであることを確認済み");
            if meta.dirty {
                let frame = self.lock_frame(i);
                if let Some(page) = frame.page.as_ref() {
                    self.disk.write_page(page)?;
                }
            }
            self.lock_frame(i).page = None;
            inner.page_table.remove(&evicted_id);
            return Ok(i);
        }
        Err(DbError::BufferPoolFull(
            "全フレームがpin中のため、evictできるページがありません".to_string(),
        ))
    }

    /// `frame_id`のフレーム本体を1つロックする(メタデータではなくページ本体)。
    fn lock_frame(&self, frame_id: usize) -> MutexGuard<'_, Frame> {
        self.frames[frame_id]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `frame_id`のpinを1つ外す。`PageReadGuard`・`PageWriteGuard`の`Drop`から
    /// 呼ばれる。
    fn unpin(&self, frame_id: usize, mark_dirty: bool) {
        let mut inner = self.lock_inner();
        let meta = &mut inner.meta[frame_id];
        meta.pin_count = meta.pin_count.saturating_sub(1);
        if mark_dirty {
            meta.dirty = true;
        }
    }
}

/// [`BufferPool::read_page`]が返す、読み取り専用のRAII Guard。
///
/// このGuardが生きている間、対象のページはpinされていてevictされない。
/// `Drop`されるとpinが1つ外れる。dirty flagは立てない。
pub struct PageReadGuard<'a> {
    pool: &'a BufferPool,
    frame_id: usize,
    page_id: PageId,
    guard: MutexGuard<'a, Frame>,
}

impl PageReadGuard<'_> {
    /// このGuardが指すページのID。
    pub fn page_id(&self) -> PageId {
        self.page_id
    }

    /// このGuardが指すページの種類。
    pub fn page_type(&self) -> PageType {
        self.page().page_type
    }

    /// ページ本体(`payload`)への読み取り専用アクセス。
    pub fn data(&self) -> &[u8] {
        self.page().payload()
    }

    fn page(&self) -> &Page {
        self.guard
            .page
            .as_ref()
            .expect("pin中のフレームは必ずページを保持している")
    }
}

impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        self.pool.unpin(self.frame_id, false);
    }
}

/// [`BufferPool::write_page`]が返す、書き込み用のRAII Guard。
///
/// `PageReadGuard`との違いは、ページ本体への可変アクセス(`data_mut`)を
/// 提供する点と、`Drop`されるときにdirty flagを立てる点である。実際に
/// バイト列を変更したかどうかにかかわらず、`write_page`でpinしたページは
/// Dropの時点でdirtyとして扱う。これは、フレーム単位で変更範囲を追跡する
/// より精密な仕組みを持たない、この章の割り切りである。
pub struct PageWriteGuard<'a> {
    pool: &'a BufferPool,
    frame_id: usize,
    page_id: PageId,
    guard: MutexGuard<'a, Frame>,
}

impl PageWriteGuard<'_> {
    /// このGuardが指すページのID。
    pub fn page_id(&self) -> PageId {
        self.page_id
    }

    /// このGuardが指すページの種類。
    pub fn page_type(&self) -> PageType {
        self.page().page_type
    }

    /// ページ本体(`payload`)への読み取り専用アクセス。
    pub fn data(&self) -> &[u8] {
        self.page().payload()
    }

    /// ページ本体(`payload`)への可変アクセス。
    pub fn data_mut(&mut self) -> &mut [u8] {
        self.page_mut().payload_mut()
    }

    fn page(&self) -> &Page {
        self.guard
            .page
            .as_ref()
            .expect("pin中のフレームは必ずページを保持している")
    }

    fn page_mut(&mut self) -> &mut Page {
        self.guard
            .page
            .as_mut()
            .expect("pin中のフレームは必ずページを保持している")
    }
}

impl Drop for PageWriteGuard<'_> {
    fn drop(&mut self) {
        self.pool.unpin(self.frame_id, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-buffer-pool-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        path
    }

    /// あらかじめ`n`枚のデータページを割り当てただけの`DiskManager`を作る。
    fn disk_with_pages(path: &std::path::Path, n: u64) -> DiskManager {
        let disk = DiskManager::open(path).unwrap();
        for _ in 0..n {
            disk.allocate_page(PageType::Data).unwrap();
        }
        disk
    }

    #[test]
    fn second_reference_to_the_same_page_is_a_hit() {
        let path = temp_path("hit-miss");
        let disk = disk_with_pages(&path, 1);
        let pool = BufferPool::new(disk, 4);

        {
            let _g = pool.read_page(PageId(1)).unwrap();
        }
        {
            let _g = pool.read_page(PageId(1)).unwrap();
        }

        let stats = pool.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 1);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn referencing_the_same_page_repeatedly_without_a_pool_costs_disk_io_every_time() {
        // 「前章の限界」の再現: BufferPoolを挟まずDiskManagerを直接叩くと、
        // 同じページへの参照のたびにディスクI/Oが発生する。
        let path = temp_path("no-pool-io");
        let disk = disk_with_pages(&path, 1);

        for _ in 0..50 {
            let _page = disk.read_page(PageId(1)).unwrap();
        }
        assert_eq!(disk.io_count(), 50);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn buffer_pool_avoids_disk_io_for_repeated_references() {
        // 同じ参照パターンをBufferPool経由で行うと、ディスクI/Oは最初の1回だけ。
        let path = temp_path("pool-avoids-io");
        let disk = disk_with_pages(&path, 1);
        let pool = BufferPool::new(disk, 4);

        for _ in 0..50 {
            let _g = pool.read_page(PageId(1)).unwrap();
        }

        let stats = pool.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 49);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn exceeding_capacity_evicts_a_page() {
        let path = temp_path("eviction");
        let disk = disk_with_pages(&path, 3);
        let pool = BufferPool::new(disk, 2);

        {
            let _g1 = pool.read_page(PageId(1)).unwrap();
        }
        {
            let _g2 = pool.read_page(PageId(2)).unwrap();
        }
        // 容量2のところへ3枚目を読み込むと、1・2のどちらかがevictされる。
        {
            let _g3 = pool.read_page(PageId(3)).unwrap();
        }
        // evictされたページは、もう一度参照するとミスになる(=どちらかは
        // キャッシュから消えている)。
        let stats_before = pool.stats();
        {
            let _g1_again = pool.read_page(PageId(1)).unwrap();
        }
        let stats_after = pool.stats();
        assert_eq!(stats_after.misses, stats_before.misses + 1);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn dirty_page_is_written_back_on_eviction() {
        let path = temp_path("dirty-writeback");
        let disk = disk_with_pages(&path, 3);
        let pool = BufferPool::new(disk, 2);

        {
            let mut g = pool.write_page(PageId(1)).unwrap();
            g.data_mut()[0..5].copy_from_slice(b"alice");
        }
        // 容量2のプールへ2枚読み込んでpage 1を押し出す。
        {
            let _g2 = pool.read_page(PageId(2)).unwrap();
        }
        {
            let _g3 = pool.read_page(PageId(3)).unwrap();
        }

        // page 1を読み直すと、evict時に書き戻された変更が読める。
        let g1 = pool.read_page(PageId(1)).unwrap();
        assert_eq!(&g1.data()[0..5], b"alice");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn pinning_beyond_capacity_returns_buffer_pool_full() {
        let path = temp_path("all-pinned");
        let disk = disk_with_pages(&path, 3);
        let pool = BufferPool::new(disk, 2);

        let g1 = pool.read_page(PageId(1)).unwrap();
        let g2 = pool.read_page(PageId(2)).unwrap();

        match pool.read_page(PageId(3)) {
            Err(DbError::BufferPoolFull(_)) => {}
            Ok(_) => panic!("全フレームがpin中のはずなのに読み込めてしまいました"),
            Err(other) => panic!("BufferPoolFullを期待しましたが別のエラーでした: {other}"),
        }

        drop(g1);
        drop(g2);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn dropping_a_guard_unpins_and_frees_a_slot_for_eviction() {
        let path = temp_path("drop-unpins");
        let disk = disk_with_pages(&path, 3);
        let pool = BufferPool::new(disk, 2);

        let g1 = pool.read_page(PageId(1)).unwrap();
        let g2 = pool.read_page(PageId(2)).unwrap();
        assert!(pool.read_page(PageId(3)).is_err());

        // g1をdropしてpinを外すと、その分の枠でpage 3を読み込めるようになる。
        drop(g1);
        let g3 = pool.read_page(PageId(3)).unwrap();

        drop(g2);
        drop(g3);
        std::fs::remove_file(&path).unwrap();
    }
}
