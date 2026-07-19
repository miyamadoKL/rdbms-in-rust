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
//! # 第35章: フレーム本体を`RwLock`にし、Latchとして使う
//!
//! 第34章まで、フレーム本体(`Frame`)は`Mutex`で守っていた。`Mutex`は
//! Exclusiveの区別しか持たないため、同じページを読むだけの2つの
//! [`PageReadGuard`]であっても、片方が生きている間はもう片方の`read_page`が
//! ロック待ちで止まっていた(第14章の演習問題2、および本章の本文
//! 「Read Latchの共存」を参照)。この章から`frames`の要素を`RwLock<Frame>`に
//! 変え、`read_page`は`RwLock::read`、`write_page`と`evict`・`flush_frame`は
//! `RwLock::write`を取るようにした。この`RwLock<Frame>`こそが、この章が導入
//! する**Latch**の実体である。ページの中身を保護する主体は第14章から変わって
//! いない。変わったのは、読み取り同士を同時に許すという一点だけである。
//!
//! # Latchの取得順序: Inner(メタデータ)を先に、Frame(ページ本体)をあとに
//!
//! `BufferPool`は2種類のロックを持つ。`page_table`・`pin_count`等をまとめた
//! `Mutex<Inner>`(メタデータ)と、フレームごとの`RwLock<Frame>`(ページ本体、
//! 上述のLatch)である。この2つを同時に取る箇所(`locate_or_load`・`evict`)は
//! すべて「`Inner`を先にロックし、その`MutexGuard`を握ったままFrameのLatchを
//! 取る」という順序で統一している。逆順(Frameを先に、Innerをあと)を許すと、
//! スレッドAが`page_table`を調べる(Inner確保)ためにevictを試み、evict先の
//! フレームがスレッドBの`PageReadGuard`によってLatch中で待たされている間に、
//! スレッドBがそのGuardを`Drop`する段になって(Frameをまだ握ったまま)
//! `unpin`のために`Inner`を取ろうとすると、AがInnerを握ったままB保有の
//! Frameを待ち、BがFrameを握ったままAが握るInnerを待つ、という循環待ちが
//! 起こりうる。
//!
//! 実際、[`PageReadGuard`]・[`PageWriteGuard`]の`Drop`は素朴に書くとこの逆順
//! を踏む。`Drop`の中で`self.pool.unpin(...)`(Inner確保)を呼んだあと、
//! Rustは構造体のフィールドを宣言順に自動でdropする。つまり素朴な実装では
//! 「Inner確保 → (自動drop完了後に)Frame解放」という順序になり、`unpin`の
//! 実行中は依然としてFrame Latchを握ったままInnerを取りに行くことになる。
//! これは上で述べた逆順そのものであり、Buffer Poolを複数スレッドから使う
//! 途端にデッドロックしうる。この章では`guard`フィールドを
//! `std::mem::ManuallyDrop`で包み、`Drop::drop`の中で明示的に
//! `ManuallyDrop::drop(&mut self.guard)`を呼んでFrame Latchを先に解放して
//! から`unpin`(Inner確保)を呼ぶよう順序を固定した。「Frameを先に手放して
//! からInnerに触る」は「Innerを先に、Frameをあとに」という規律に反して
//! いるように見えるが、この2つは同時に保持されることが無くなった(Frameを
//! 解放し終えてからInnerを取るだけ)という点で、規律が禁じる「逆順で同時に
//! 保持する」状態そのものを作らない。
//!
//! この規律は、B+Tree(`crate::btree`、第35章)のLock Couplingが従う
//! 「常に上から下、左から右」という取得順序とは別の軸の規律である。B+Treeの
//! 規律はページとページの間の順序を、この規律はBuffer Pool内部のInnerと
//! フレームという2種類のロックの間の順序を決める。

use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use crate::disk_manager::DiskManager;
use crate::error::{DbError, DbResult};
use crate::ids::{Lsn, PageId};
use crate::page::{Page, PageType};
use crate::wal::WalWriter;

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
    /// **Page LSN**(第33章)。このページへの変更のうち、対応するWALレコードが
    /// 存在する最新のものの`Lsn`。`Lsn(0)`は「WALに追跡された変更がまだ無い」
    /// ことを表す番兵で、`crate::wal::Lsn`が実際に払い出す最小値(`Lsn(1)`)と
    /// 衝突しない。dirtyなページをディスクへ書き戻す直前、この値までWALが
    /// 同期済みであることを保証する(`BufferPool::flush_frame`・`evict`の
    /// ドキュメントを参照)。
    ///
    /// # 第34章での変更: ページ自身へも永続化する
    ///
    /// 第33章までは、この値はこの`FrameMeta`(プロセスのメモリ上)にしか
    /// 存在しなかった。この章から`crate::page::Page`自身も同じ`page_lsn`を
    /// 持つようになり、ページをディスクへ書き戻す(`flush_frame`・`evict`)
    /// 直前にこのメタデータの値を`Page::page_lsn`へ書き写してから
    /// `DiskManager::write_page`を呼ぶ。ページを新しく読み込む
    /// (`locate_or_load`)ときは、逆にディスクから読んだ`Page::page_lsn`を
    /// この`FrameMeta::page_lsn`の初期値として引き継ぐ(`Lsn(0)`で固定的に
    /// 初期化していた第33章までとの違い)。これにより、プロセスを再起動して
    /// 読み直したページも、クラッシュ前にどこまでWALが反映されていたかを
    /// 正しく覚えている状態から始まる。`crate::recovery::recover`のRedoが
    /// 「このページのこの変更は、もうディスクに届いているか」を判定する
    /// 材料は、まさにこの値である。
    page_lsn: Lsn,
}

impl FrameMeta {
    fn empty() -> Self {
        FrameMeta {
            occupant: None,
            pin_count: 0,
            dirty: false,
            referenced: false,
            page_lsn: Lsn(0),
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
    /// WALファースト不変条件(第33章)を強制するために参照するWAL。
    /// `crate::storage::Storage`が`create`・`open`のあとで
    /// [`BufferPool::attach_wal`]を呼び、テーブル本体用の`BufferPool`にだけ
    /// 結線する(索引ごとの`BufferPool`には結線しない、本文「この章が
    /// 対象にする範囲」を参照)。`None`のままなら、このBufferPoolは
    /// 第14章までと同じ、WALを一切意識しない書き戻しを行う。
    wal: Option<Arc<Mutex<WalWriter>>>,
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
    /// `RwLock`がこの章のLatchの実体である(モジュール冒頭を参照)。
    frames: Vec<RwLock<Frame>>,
    inner: Mutex<Inner>,
}

impl BufferPool {
    /// `disk`をキャッシュ元とし、`capacity`フレーム分のページを保持できる
    /// `BufferPool`を作る。
    ///
    /// `capacity`は0より大きい必要がある(0だとどのページも読み込めない)。
    pub fn new(disk: DiskManager, capacity: usize) -> Self {
        assert!(capacity > 0, "capacityは1以上である必要があります");
        let frames = (0..capacity).map(|_| RwLock::new(Frame { page: None })).collect();
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
                wal: None,
            }),
        }
    }

    /// このBufferPoolが保持できるフレーム数。
    pub fn capacity(&self) -> usize {
        self.frames.len()
    }

    /// このBufferPoolにWALを結線し、以後dirtyなページの書き戻し前に
    /// WALファースト不変条件を強制するようにする(第33章)。
    ///
    /// `Storage::create`・`Storage::open`が、テーブル本体用の`BufferPool`に
    /// 対してだけ1回呼ぶ。呼ばなければ、このBufferPoolは第14章までと同じ
    /// 挙動のままになる(索引専用の`BufferPool`はこの章では呼ばない、
    /// モジュール冒頭の`Inner::wal`のドキュメントを参照)。
    pub fn attach_wal(&self, wal: Arc<Mutex<WalWriter>>) {
        self.lock_inner().wal = Some(wal);
    }

    /// `id`のページが今このBufferPoolに読み込まれていれば、その**Page LSN**
    /// (第33章)を`lsn`まで引き上げる(すでにより新しい`lsn`が記録されていれば
    /// 何もしない)。
    ///
    /// `crate::storage::Storage`の`insert`・`update`・`delete`が、対応する
    /// WALレコードを`append`した直後に呼ぶ。呼び出し時点でそのページは
    /// 直前の書き込みによって必ずこのBufferPoolに読み込まれているはずなので、
    /// 見つからない場合は何もしない(呼び出し側のバグを示す可能性はあるが、
    /// このメソッド自身は`&self`しか取らない薄い更新であり、ここで
    /// panicするほどの不変条件はまだ無い)。
    pub fn bump_page_lsn(&self, id: PageId, lsn: Lsn) {
        let mut inner = self.lock_inner();
        if let Some(&frame_id) = inner.page_table.get(&id) {
            let meta = &mut inner.meta[frame_id];
            if lsn > meta.page_lsn {
                meta.page_lsn = lsn;
            }
        }
    }

    /// `id`のページの現在のPage LSN(第34章)を返す。
    ///
    /// まだキャッシュされていなければ`DiskManager`から読み込む(その時点で
    /// ディスクに永続化されている値を引き継ぐ、`locate_or_load`を参照)。
    /// `crate::recovery::recover`のRedoが、あるログレコードをこのページへ
    /// 再適用すべきかどうか(`このLsn < レコードのLsn`)を判定するために使う。
    pub(crate) fn page_lsn(&self, id: PageId) -> DbResult<Lsn> {
        let frame_id = self.locate_and_pin(id)?;
        let lsn = self.lock_inner().meta[frame_id].page_lsn;
        self.unpin(frame_id, false);
        Ok(lsn)
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
    ///
    /// 取得するのはこのフレームのRead Latch(`RwLock::read`)であり、同じ
    /// ページを指す他のスレッドの`PageReadGuard`と共存できる。`write_page`が
    /// 握るWrite Latchとだけ両立しない(モジュール冒頭を参照)。
    pub fn read_page(&self, id: PageId) -> DbResult<PageReadGuard<'_>> {
        let frame_id = self.locate_and_pin(id)?;
        let guard = ManuallyDrop::new(self.lock_frame_read(frame_id));
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
        let guard = ManuallyDrop::new(self.lock_frame_write(frame_id));
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
    /// [`BufferPool::sync`]を別途呼ぶ必要がある。`disk`はこの`BufferPool`が
    /// privateフィールドとして所有しているため、呼び出し側が`DiskManager`へ
    /// 直接触れる経路はなく、`sync`まで行いたい場合は必ずこのメソッドを経由する。
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

    /// 保持している`DiskManager`に対して`sync`を呼び、OSにディスクへの実際の
    /// 反映を要求する。
    ///
    /// `flush_all`はキャッシュされたページをOSへ書き渡すところまでしか行わない
    /// ため、プロセスの再起動をまたいでデータを残すにはこの`sync`まで呼ぶ必要が
    /// ある(`DiskManager::sync`のドキュメントを参照)。`disk`はこの`BufferPool`
    /// が単独で所有しており外部から触れられないため、この`sync`が
    /// `DiskManager::sync`を呼べる唯一の経路になる。
    pub fn sync(&self) -> DbResult<()> {
        self.disk.sync()
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
    ///
    /// # WALファースト不変条件の強制
    ///
    /// 実際にページを書き戻す(`self.disk.write_page`)前に、このフレームの
    /// Page LSNを読み、WALが結線されていれば[`WalWriter::sync_up_to`]を呼ぶ。
    /// これにより、このページに反映されている変更を表すWALレコードは、
    /// ページ自身よりも必ず先にディスクへ同期される。WALのロックとフレームの
    /// ロックはこの手順の中で同時に保持しない(WALの同期を終えてから
    /// フレームをロックする)ため、`WalWriter`側の処理が長くかかっても
    /// このBufferPoolの他の操作をブロックしない。
    fn flush_frame(&self, frame_id: usize) -> DbResult<()> {
        let (page_lsn, wal) = {
            let inner = self.lock_inner();
            (inner.meta[frame_id].page_lsn, inner.wal.clone())
        };
        if let Some(wal) = wal {
            wal.lock().unwrap_or_else(|p| p.into_inner()).sync_up_to(page_lsn)?;
        }

        let mut frame = self.lock_frame_write(frame_id);
        if let Some(page) = frame.page.as_mut() {
            // 第34章: このフレームのPage LSNをページ自身へ書き写してから
            // ディスクへ書き戻す。こうしておかないと、次にこのページを読み込む
            // (クラッシュ後の`Storage::open`も含む)側が、どこまでの変更が
            // すでに反映済みかを知る手段を失う(`FrameMeta::page_lsn`の
            // 「第34章での変更」を参照)。
            page.page_lsn = page_lsn;
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
        // 第34章: ディスクに永続化されている`page_lsn`を、そのままこの
        // フレームの初期値として引き継ぐ(モジュール冒頭`FrameMeta::page_lsn`の
        // 「第34章での変更」を参照)。`Lsn(0)`固定で初期化していた第33章までとの違い。
        let page_lsn = page.page_lsn;
        self.lock_frame_write(frame_id).page = Some(page);
        inner.meta[frame_id] = FrameMeta {
            occupant: Some(id),
            pin_count: 0,
            dirty: false,
            referenced: false,
            page_lsn,
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
    /// dirtyなフレームを書き戻す直前にWALファースト不変条件を強制する点は
    /// [`BufferPool::flush_frame`]と同じである(`Inner::wal`のドキュメントを参照)。
    fn evict(&self, inner: &mut Inner) -> DbResult<usize> {
        let capacity = self.frames.len();
        let wal = inner.wal.clone();
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
            let dirty = meta.dirty;
            let page_lsn = meta.page_lsn;
            if dirty {
                if let Some(wal) = &wal {
                    wal.lock().unwrap_or_else(|p| p.into_inner()).sync_up_to(page_lsn)?;
                }
                let mut frame = self.lock_frame_write(i);
                if let Some(page) = frame.page.as_mut() {
                    // `flush_frame`と同じ理由でPage LSNをページ自身へ書き写す
                    // (第34章)。
                    page.page_lsn = page_lsn;
                    self.disk.write_page(page)?;
                }
            }
            self.lock_frame_write(i).page = None;
            inner.page_table.remove(&evicted_id);
            return Ok(i);
        }
        Err(DbError::BufferPoolFull(
            "全フレームがpin中のため、evictできるページがありません".to_string(),
        ))
    }

    /// `frame_id`のフレーム本体にRead Latchをかける(メタデータではなく
    /// ページ本体)。同じフレームの他の`PageReadGuard`とは共存できるが、
    /// `lock_frame_write`とは両立しない。
    fn lock_frame_read(&self, frame_id: usize) -> RwLockReadGuard<'_, Frame> {
        self.frames[frame_id]
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `frame_id`のフレーム本体にWrite Latchをかける。他のどの`PageReadGuard`
    /// ・`PageWriteGuard`とも同時には持てない。
    fn lock_frame_write(&self, frame_id: usize) -> RwLockWriteGuard<'_, Frame> {
        self.frames[frame_id]
            .write()
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
    /// `ManuallyDrop`で包み、`Drop::drop`の中で明示的にFrame Latchを解放して
    /// から`unpin`(Inner確保)を呼べるようにしている(モジュール冒頭の
    /// 「Latchの取得順序」を参照)。
    guard: ManuallyDrop<RwLockReadGuard<'a, Frame>>,
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
        // Frame Latchを先に解放してから`unpin`(Inner確保)を呼ぶ。逆順だと
        // Inner確保中もFrame Latchを握り続けることになり、evictと循環待ちに
        // なりうる(モジュール冒頭の「Latchの取得順序」を参照)。
        // SAFETY: `guard`はこの後この構造体が読まれることはなく、二重dropも
        // 起きない(構造体自体が`Drop::drop`を抜けたあとフィールドの自動drop
        // 対象から外れるのが`ManuallyDrop`の意味である)。
        unsafe {
            ManuallyDrop::drop(&mut self.guard);
        }
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
    /// `PageReadGuard`と同じ理由で`ManuallyDrop`に包む。
    guard: ManuallyDrop<RwLockWriteGuard<'a, Frame>>,
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

    /// このページの`PageType`を書き換える(第35章)。
    ///
    /// `BTree::grow_new_root`が、Rootの`PageId`を変えずに(古いRootページを
    /// そのまま)Leaf PageからInternal Pageへ育てるために使う。通常の
    /// ページはすべて`allocate_page`が決めた`PageType`のまま生涯変わらない
    /// ため、この操作を使うのはその1箇所だけを想定している。
    pub fn set_page_type(&mut self, page_type: PageType) {
        self.page_mut().page_type = page_type;
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
        // SAFETY: `PageReadGuard::drop`と同じ理由。
        unsafe {
            ManuallyDrop::drop(&mut self.guard);
        }
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

    fn wal_temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-buffer-pool-wal-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        path
    }

    /// WALファースト不変条件(第33章)の核心: `attach_wal`したBufferPoolが
    /// dirtyなページを書き戻す(evictする、または`flush_page`を呼ぶ)前に、
    /// そのページのPage LSNまでWALが必ず同期されている。
    #[test]
    fn flush_page_syncs_the_wal_up_to_the_pages_page_lsn_before_writing_it_back() {
        let db_path = temp_path("wal-first-flush");
        let wal_path = wal_temp_path("wal-first-flush");
        let disk = disk_with_pages(&db_path, 1);
        let pool = BufferPool::new(disk, 4);
        let wal = Arc::new(Mutex::new(WalWriter::open(&wal_path).unwrap()));
        pool.attach_wal(wal.clone());

        let lsn = {
            let mut w = wal.lock().unwrap();
            w.append_begin(crate::ids::TransactionId(1))
        };
        {
            let mut g = pool.write_page(PageId(1)).unwrap();
            g.data_mut()[0..5].copy_from_slice(b"alice");
        }
        pool.bump_page_lsn(PageId(1), lsn);

        // まだ`sync_up_to`を誰も呼んでいないので、WALはまだこのlsnまで
        // 同期されていない。
        assert!(wal.lock().unwrap().durable_lsn() < lsn);

        pool.flush_page(PageId(1)).unwrap();

        // `flush_page`がページを書き戻す前に、必ずこのlsnまでWALを
        // 同期しているはず。
        assert!(wal.lock().unwrap().durable_lsn() >= lsn);

        std::fs::remove_file(&db_path).unwrap();
        std::fs::remove_file(&wal_path).unwrap();
    }

    /// [`flush_page_syncs_the_wal_up_to_the_pages_page_lsn_before_writing_it_back`]
    /// と同じ不変条件を、`flush_page`ではなくClock置換によるevict経路
    /// (`BufferPool::evict`)でも確認する。
    #[test]
    fn eviction_syncs_the_wal_up_to_the_evicted_pages_page_lsn_before_writing_it_back() {
        let db_path = temp_path("wal-first-evict");
        let wal_path = wal_temp_path("wal-first-evict");
        let disk = disk_with_pages(&db_path, 2);
        let pool = BufferPool::new(disk, 1);
        let wal = Arc::new(Mutex::new(WalWriter::open(&wal_path).unwrap()));
        pool.attach_wal(wal.clone());

        let lsn = {
            let mut w = wal.lock().unwrap();
            w.append_begin(crate::ids::TransactionId(1))
        };
        {
            let mut g = pool.write_page(PageId(1)).unwrap();
            g.data_mut()[0..5].copy_from_slice(b"alice");
        }
        pool.bump_page_lsn(PageId(1), lsn);
        assert!(wal.lock().unwrap().durable_lsn() < lsn);

        // 容量1のプールへpage 2を読み込むと、page 1がevictされ書き戻される。
        {
            let _g2 = pool.read_page(PageId(2)).unwrap();
        }
        assert!(wal.lock().unwrap().durable_lsn() >= lsn);

        std::fs::remove_file(&db_path).unwrap();
        std::fs::remove_file(&wal_path).unwrap();
    }
}
