//! テーブル定義とデータページの両方を1つのファイルへ永続化するストレージエンジン。
//!
//! 第13章の`HeapFile`は「1つの`BufferPool`(1つのファイル)は丸ごと1個の
//! テーブルが占有する」という前提で作られていた。`HeapFile::open`は、ページ0
//! (Metaページ)を除く全ページを問答無用でそのテーブルのデータページとみなす。
//! この前提のもとでは、2つ目のテーブルを同じファイルに置けない。もう1つ、
//! この前提には表に出ていない欠陥がある。テーブルが「今どのページを使っているか」
//! という情報そのものが、ファイルのどこにも書かれていない。`HeapFile::open`が
//! 正しく動くのは、そのプロセスの中でどのページが空いていて、どのページが
//! 実際にデータを持っているかを、ページの中身を全部読み直すことで(結果的に)
//! 復元できていたからにすぎない。
//!
//! `Storage`は、この2つの限界を解消する。テーブル定義・各テーブルの
//! `page_ids`・削除済みテーブルのページの再利用リストを**Catalogページ**という
//! 固定位置のページへ永続化し、複数のテーブルを1つのファイルに同居させる。
//!
//! # Catalogページはページ1に固定する
//!
//! ページ0はすでに第11章のFile Headerが占有している。カタログを置く場所を
//! 自由に選べるなら、カタログ自身をどこに置いたかという情報を、また別のどこかに
//! 書いておかなければならない。この堂々巡り(カタログを見ないと場所が分からず、
//! 場所が分からないとカタログを見られない)を断ち切る方法は、File Headerが
//! ページ0を名乗ったのと同じ発想である。あらかじめ決めた固定のページ番号に
//! カタログを置き、「そこを見ればいつでもカタログがある」という前提そのものを
//! コードに埋め込んでしまう。この章では、空いている最初の番号であるページ1を
//! Catalogページの定位置とする。
//!
//! # Catalogページのレイアウト
//!
//! Catalogページの`payload`(`PAGE_PAYLOAD_SIZE`バイト)には、次の内容を
//! 手書きのリトルエンディアンでエンコードする。
//!
//! ```text
//! next_table_id:    u64
//! table_count:      u32
//! free_page_count:  u32
//! free_page_ids:    u64 × free_page_count
//! tables × table_count:
//!     table_id:       u64
//!     name_len:       u16
//!     name:           u8 × name_len
//!     column_count:   u16
//!     columns × column_count:
//!         col_name_len: u16
//!         col_name:     u8 × col_name_len
//!         data_type:    u8 (0=BOOLEAN, 1=BIGINT, 2=TEXT)
//!         nullable:     u8 (0 または 1)
//!     page_count:     u32
//!     page_ids:       u64 × page_count
//! ```
//!
//! `next_table_id`は、第9章の`Catalog`が`next_table_id: u64`をメモリ上だけに
//! 持っていたのと同じ役割を、再起動をまたいで担う。これを永続化しないと、
//! テーブルを作って削除して再起動しただけで空いた番号が使い回され、「削除後も
//! `TableId`は再利用しない」という第9章からの不変条件が再起動のたびに崩れる。
//!
//! 各テーブルの`page_ids`をカタログへそのまま書き出す設計は、テーブルが
//! ページを何十万枚も抱えるようになると破綻する。ページ番号1つが8バイトなので、
//! ページ数が増えるほどそのテーブルのカタログ上の専有量も線形に増え、
//! いずれCatalogページ1枚(`PAGE_PAYLOAD_SIZE`バイト)に収まらなくなる。
//! この章はその場合を複数ページへの分割では解決せず、`DbError::CatalogTooLarge`
//! を返すという単純な割り切りにとどめる。カタログを複数ページにまたがらせる
//! (たとえばテーブルごとに専用のカタログエントリページを持たせる)構成は、
//! 章末の演習で扱う。
//!
//! # Free Page Listでページを使い回す
//!
//! `DROP TABLE`されたテーブルの`page_ids`は、ファイルからは消えない
//! (`DiskManager`にページを手放す手段がない)。その代わり、`free_pages`
//! (Free Page List)という一覧に積んでおき、次にどれかのテーブルが新しいページを
//! 必要としたとき、`pool.allocate_page`でファイルを伸ばすより先にこの一覧から
//! 1枚取り出して再利用する。取り出したページは`SlottedPage::init`で
//! 作り直してから使うため、以前どのテーブルの、どんな中身のデータが入っていたかは
//! 一切引き継がない。
//!
//! # Free Space Mapで線形探索のI/Oを減らす
//!
//! `insert`が空きのあるページを探す部分は、[`crate::free_space_map`]の
//! `FreeSpaceMap`が担う。この章のFree Space Mapの粒度・更新タイミングの設計は
//! そのモジュールのドキュメントを参照。

use std::collections::HashMap;
use std::path::Path;

use crate::buffer_pool::BufferPool;
use crate::catalog::TableInfo;
use crate::disk_manager::DiskManager;
use crate::error::{DbError, DbResult};
use crate::free_space_map::FreeSpaceMap;
use crate::heap_file::Scan;
use crate::ids::{PageId, RecordId, TableId};
use crate::page::{PAGE_PAYLOAD_SIZE, PageType};
use crate::slotted_page::{SlotStatus, SlottedPage, SlottedPageRef, max_len_for_fresh_page};
use crate::types::{Column, DataType, Schema};

/// Catalogページの定位置。ページ0はFile Header(第11章)が占有しているため、
/// 空いている最初の番号を使う。
const CATALOG_PAGE_ID: PageId = PageId(1);

/// `Storage`が内部で使う`BufferPool`の容量。
///
/// この章では、呼び出し側にバッファプールのサイズを選ばせる引数を公開しない
/// (`Storage::create`・`Storage::open`はどちらも`path`だけを受け取る)。容量を
/// 調整可能にする拡張は、それを必要とする章(あるいは章末の演習)に譲る。
const DEFAULT_BUFFER_POOL_CAPACITY: usize = 64;

/// カタログに登録された1テーブルの情報と、そのテーブルが使っているページの一覧。
struct TableEntry {
    info: TableInfo,
    /// このテーブルが使っているデータページの一覧。第13章の`HeapFile::page_ids`と
    /// 同じ役割だが、こちらはCatalogページを介して永続化されている。
    page_ids: Vec<PageId>,
}

/// テーブル定義とデータページの両方を1つのファイルへ永続化するストレージエンジン。
pub struct Storage {
    pool: BufferPool,
    next_table_id: u64,
    tables: HashMap<TableId, TableEntry>,
    /// `DROP TABLE`によって空いた、再利用待ちのページの一覧。
    free_pages: Vec<PageId>,
    fsm: FreeSpaceMap,
}

impl Storage {
    /// `path`に新しいストレージファイルを作る。
    ///
    /// `path`がまだ存在しない、またはFile Headerだけを持つ空のファイル
    /// (`page_count == 1`)であれば、Catalogページを1枚確保して初期化する。
    /// すでにCatalogページを持つファイル(`page_count > 1`)に対して呼ぶと、
    /// 既存の内容を壊して初期化してしまわないよう`DbError::CorruptPage`を返す。
    /// 既存のファイルを開きたい場合は[`Storage::open`]を使う。
    pub fn create<P: AsRef<Path>>(path: P) -> DbResult<Self> {
        let disk = DiskManager::open(path)?;
        if disk.page_count() > 1 {
            return Err(DbError::CorruptPage(
                "既に初期化されたファイルです(Storage::createではなくStorage::openを使ってください)"
                    .to_string(),
            ));
        }

        let pool = BufferPool::new(disk, DEFAULT_BUFFER_POOL_CAPACITY);
        let catalog_page_id = pool.allocate_page(PageType::Catalog)?;
        debug_assert_eq!(
            catalog_page_id, CATALOG_PAGE_ID,
            "新規ファイルで最初に確保されるページは常にCatalogページの定位置になる"
        );

        let storage = Storage {
            pool,
            next_table_id: 0,
            tables: HashMap::new(),
            free_pages: Vec::new(),
            fsm: FreeSpaceMap::new(),
        };
        storage.persist_catalog()?;
        Ok(storage)
    }

    /// `path`の既存のストレージファイルを開く。
    ///
    /// `DiskManager::open`がFile HeaderのMagic Number・Format Version・checksumを
    /// 検証する(第13章)。この章ではさらに、`page_count >= 2`(Catalogページを
    /// 持つ)ことと、ページ1が実際に`PageType::Catalog`であることを確認したうえで、
    /// そのページの中身をカタログとして復元する。いずれかの検証に失敗した場合は
    /// `DbError::CorruptPage`を返す。
    ///
    /// カタログのバイト列自体は`decode_catalog`で読めても、その中身が意味を
    /// なさない場合がある。範囲外の`PageId`、Meta/Catalogという予約ページへの
    /// 参照、あるページが複数のテーブル(またはFree Page List)に同時に属している、
    /// テーブルが実際には`PageType::Data`ではないページを指している、といった
    /// 矛盾はどれもバイト列としては正しく読めてしまうため、`decode_catalog`の
    /// 構造検査だけでは捕まらない。`open`はこれらを`fsm`を組み立てる前に検証し、
    /// 見つかった場合は`DbError::CorruptCatalog`を返す。この検証がないと、
    /// たとえば`free_pages`にMetaページ(`PageId(0)`)が紛れ込んだカタログを
    /// そのまま受理してしまい、次の`insert`がそのページを「空きページ」として
    /// 再利用してFile Headerを上書きし、以後`open`できないファイルを作ってしまう。
    pub fn open<P: AsRef<Path>>(path: P) -> DbResult<Self> {
        let disk = DiskManager::open(path)?;
        if disk.page_count() < 2 {
            return Err(DbError::CorruptPage(
                "Catalogページがありません(Storage::createで作成したファイルではありません)"
                    .to_string(),
            ));
        }

        let pool = BufferPool::new(disk, DEFAULT_BUFFER_POOL_CAPACITY);

        let guard = pool.read_page(CATALOG_PAGE_ID)?;
        if guard.page_type() != PageType::Catalog {
            return Err(DbError::CorruptPage(format!(
                "PageId({})はCatalogページである必要がありますが{:?}でした",
                CATALOG_PAGE_ID.0,
                guard.page_type()
            )));
        }
        let decoded = decode_catalog(guard.data())?;
        drop(guard);

        validate_table_metadata(&decoded)?;

        // Free Space Mapは永続化しない(モジュール冒頭の説明を参照)。カタログから
        // 復元した各ページを1回ずつ読み、意味検証(範囲・予約ページ・共有・
        // PageType)を行いながら、実測の free_space() から fsm を作り直す。
        let page_count = pool.page_count();
        let mut claimed_pages: std::collections::HashSet<PageId> = std::collections::HashSet::new();
        let mut fsm = FreeSpaceMap::new();

        for &page_id in &decoded.free_pages {
            claim_page(page_id, page_count, &mut claimed_pages)?;
            let guard = pool.read_page(page_id)?;
            if guard.page_type() != PageType::Data {
                return Err(DbError::CorruptCatalog(format!(
                    "Free Page List中のPageId({})はPageType::Dataである必要がありますが{:?}でした",
                    page_id.0,
                    guard.page_type()
                )));
            }
        }

        for entry in decoded.tables.values() {
            for &page_id in &entry.page_ids {
                claim_page(page_id, page_count, &mut claimed_pages)?;
                let guard = pool.read_page(page_id)?;
                if guard.page_type() != PageType::Data {
                    return Err(DbError::CorruptCatalog(format!(
                        "TableId({})のPageId({})はPageType::Dataである必要がありますが{:?}でした",
                        entry.info.id.0,
                        page_id.0,
                        guard.page_type()
                    )));
                }
                let free = SlottedPageRef::open(guard.data())?.free_space();
                fsm.update(page_id, free);
            }
        }

        Ok(Storage {
            pool,
            next_table_id: decoded.next_table_id,
            tables: decoded.tables,
            free_pages: decoded.free_pages,
            fsm,
        })
    }

    /// キャッシュされているdirtyなページをすべてディスクへ書き戻す。
    ///
    /// `HeapFile::flush`(第14章)と同じく、`BufferPool::flush_all`をそのまま
    /// 呼ぶだけの薄いラッパーである。OSへの書き渡しまでで、実ディスクへの
    /// 同期([`Storage::sync`])までは行わない。
    pub fn flush(&self) -> DbResult<()> {
        self.pool.flush_all()
    }

    /// 保持している`BufferPool`(の`DiskManager`)に対して`sync`を呼び、OSに
    /// ディスクへの実際の反映を要求する。
    ///
    /// `flush`で書き渡した内容をプロセスの再起動をまたいで確実に残すには、
    /// この`sync`まで呼ぶ必要がある。`Database::flush`(第16章)は`flush`と
    /// この`sync`をこの順で両方呼ぶことで「呼び出し側からは`flush`ひとつで
    /// 耐久化が完了する」という単純な契約にしている。`flush`と`sync`を
    /// 分けているのは、`flush_all`(キャッシュの書き渡し)と`sync`(実ディスクへの
    /// 同期)がコストの異なる別の操作であり、両者を分けて呼べる余地を`Storage`の
    /// 層にも残しておくためである。fsyncのタイミングをより細かく制御する
    /// 話題(グループコミットなど)は第33章のWALで扱う。
    pub fn sync(&self) -> DbResult<()> {
        self.pool.sync()
    }

    /// テーブル名から`TableInfo`を引く。見つからなければ`None`を返す。
    pub fn table(&self, name: &str) -> Option<&TableInfo> {
        self.tables.values().find(|t| t.info.name == name).map(|t| &t.info)
    }

    /// 新しいテーブルを登録する。
    ///
    /// 同名のテーブルがすでに存在する場合は`DbError::DuplicateTable`を返す。
    /// `next_table_id`がすでに`u64::MAX`で次の`TableId`を安全に割り当てられない
    /// 場合は`DbError::TableIdSpaceExhausted`を返す(`u64`のオーバーフローに
    /// よってdebugビルドでpanicする、releaseビルドで0へ巻き戻って`TableId`の
    /// 一意性が壊れる、のどちらも避けるため)。`Storage::open`の
    /// `validate_table_metadata`がCatalogページの`next_table_id`が
    /// `u64::MAX`であることをすでに`DbError::CorruptCatalog`として拒否している
    /// ため、通常この分岐に到達するのは`u64::MAX`回`create_table`を呼び続けた
    /// 場合に限られる。ここでの`checked_add`は、その防御をすり抜けて
    /// メモリ上だけで`next_table_id`が`u64::MAX`に達した場合の二重の備えである。
    /// カタログの永続化(`persist_catalog`)に失敗した場合(たとえば
    /// `DbError::CatalogTooLarge`)は、メモリ上の登録も取り消す。カタログに
    /// 書き出せていないテーブルをメモリ上にだけ存在させておくと、次の操作で
    /// メモリとディスクの内容が食い違ってしまう。
    pub fn create_table(&mut self, name: &str, schema: Schema) -> DbResult<TableId> {
        if self.tables.values().any(|t| t.info.name == name) {
            return Err(DbError::DuplicateTable(name.to_string()));
        }

        let id = TableId(self.next_table_id);
        let next_table_id = self
            .next_table_id
            .checked_add(1)
            .ok_or(DbError::TableIdSpaceExhausted)?;
        self.next_table_id = next_table_id;
        self.tables.insert(
            id,
            TableEntry {
                info: TableInfo {
                    id,
                    name: name.to_string(),
                    schema,
                },
                page_ids: Vec::new(),
            },
        );

        if let Err(err) = self.persist_catalog() {
            self.tables.remove(&id);
            self.next_table_id = id.0;
            return Err(err);
        }
        Ok(id)
    }

    /// テーブルを削除する。
    ///
    /// 指定した名前のテーブルが存在しない場合は`DbError::TableNotFound`を返す。
    /// 成功すると、そのテーブルが使っていた全ページをFree Page Listへ積む。
    /// ページの中身自体はこの時点では書き換えない。次にそのページが
    /// (別のテーブルの`insert`によって)再利用されるとき、`SlottedPage::init`が
    /// 中身を作り直す。
    pub fn drop_table(&mut self, name: &str) -> DbResult<TableId> {
        let id = self
            .tables
            .iter()
            .find(|(_, t)| t.info.name == name)
            .map(|(id, _)| *id)
            .ok_or_else(|| DbError::TableNotFound(name.to_string()))?;

        let entry = self.tables.remove(&id).expect("直前にidの存在を確認済み");
        for &page_id in &entry.page_ids {
            self.fsm.remove(page_id);
            self.free_pages.push(page_id);
        }

        self.persist_catalog()?;
        Ok(id)
    }

    /// `bytes`を`table_id`のテーブルへ新しいタプルとして挿入し、それを指す
    /// `RecordId`を返す。
    ///
    /// 空きページの探索は3段階で行う。
    ///
    /// 1. `FreeSpaceMap`が、このテーブルが持つページの中から空きの見積もりが
    ///    十分なものを教えてくれれば、そこへ書き込む。
    /// 2. 見つからなければ、Free Page Listに再利用待ちのページがあればそれを
    ///    1枚もらい、`SlottedPage::init`で作り直してから書き込む。
    /// 3. それも無ければ、`BufferPool::allocate_page`でファイルへ新しいページを
    ///    1枚追加する。
    ///
    /// 2・3のどちらでも、このテーブルの`page_ids`が変わるためカタログを
    /// 永続化し直す。永続化に失敗した場合(`DbError::CatalogTooLarge`など)は
    /// `page_ids`への追加を取り消してエラーを返す。ただし、この時点で
    /// タプル自体はすでにそのページへ書き込まれてしまっており、割り当てた
    /// ページも巻き戻さない。どのテーブルにも属さない、書き込み済みだが
    /// カタログには載っていないページとして残る。これはこの章が採用する
    /// 素朴な割り切りである。
    ///
    /// `bytes`が空の1ページにも収まらないほど大きい(`max_len_for_fresh_page`
    /// 参照)場合は、上記の3段階のいずれにも進まず`DbError::TupleTooLarge`を
    /// 即座に返す。この事前検査が無いと、失敗するだけの`insert`のたびに
    /// Free Page Listからページを取り出したきり戻さない、あるいは
    /// `allocate_page`でファイルを1ページ伸ばしてしまい、同じ大きすぎる値を
    /// 何度も`insert`しようとするコードがファイルサイズを際限なく肥大化させる。
    pub fn insert(&mut self, table_id: TableId, bytes: &[u8]) -> DbResult<RecordId> {
        let needed = bytes.len();
        if needed > max_len_for_fresh_page(PAGE_PAYLOAD_SIZE) {
            return Err(DbError::TupleTooLarge(needed));
        }
        let existing_page_ids = self.table_entry(table_id)?.page_ids.clone();

        if let Some(page_id) = self.fsm.find_candidate(&existing_page_ids, needed)
            && let Some(rid) = self.try_insert_into_open_page(page_id, bytes)?
        {
            return Ok(rid);
        }
        // FreeSpaceMapの見積もりが実際の空きより楽観的だった場合(候補が
        // 見つかったのに`try_insert_into_open_page`が`None`を返した場合)は、
        // 下のFree Page List・新規ページの確保へ進む。単一スレッドの現在の
        // 設計では基本的に起こらないが、見積もりと実体がずれた場合に安全側へ
        // 倒れるためのフォールバックである。

        if let Some(page_id) = self.free_pages.pop() {
            match self.try_insert_into_fresh_page(page_id, bytes)? {
                Some(rid) => {
                    if let Err(err) = self.attach_page_to_table(table_id, page_id) {
                        self.free_pages.push(page_id);
                        return Err(err);
                    }
                    return Ok(rid);
                }
                None => {
                    // 上の事前検査により`bytes`は空の1ページには必ず収まるはず
                    // なので、通常はここに到達しない。万一到達しても、
                    // Free Page Listから取り出したページを取り戻し損ねて
                    // 宙に浮かせないよう、必ず押し戻しておく。
                    self.free_pages.push(page_id);
                    return Err(DbError::TupleTooLarge(needed));
                }
            }
        }

        let page_id = self.pool.allocate_page(PageType::Data)?;
        let rid = self
            .try_insert_into_fresh_page(page_id, bytes)?
            .ok_or(DbError::TupleTooLarge(needed))?;
        self.attach_page_to_table(table_id, page_id)?;
        Ok(rid)
    }

    /// `rid`が指すタプルのバイト列を返す。削除済み、またはそもそも挿入されて
    /// いなければ`None`を返す。
    pub fn get(&self, table_id: TableId, rid: RecordId) -> DbResult<Option<Vec<u8>>> {
        self.validate_rid(table_id, rid)?;
        let guard = self.pool.read_page(rid.page_id)?;
        Ok(SlottedPageRef::open(guard.data())?
            .get(rid.slot_id)
            .map(|bytes| bytes.to_vec()))
    }

    /// `rid`が指すタプルを`bytes`へ置き換える。
    ///
    /// `HeapFile::update`(第13章)と同じく、同じページに(コンパクション後も)
    /// 収まる限り同じ`RecordId`を保つ。収まらない場合は、`insert`と同じ経路
    /// (Free Space Map→Free Page List→新規ページ)で別の場所へ挿入し、それが
    /// 成功したときに限って元の行をこのページから削除する。対象が存在しなければ
    /// `Ok(None)`を返す。
    ///
    /// 「挿入してから削除する」順序は`HeapFile::update`(第13章)から引き継いだ
    /// 判断である。逆に「削除してから挿入する」順序だと、挿入が
    /// `DbError::TupleTooLarge`や`DbError::CatalogTooLarge`で失敗したときに
    /// 元の行がすでに消えてしまい、失敗した`UPDATE`が行の消失につながる。
    ///
    /// `bytes`が空の1ページにも収まらないほど大きい場合は、`insert`と同じく
    /// どのページも変更せずに`DbError::TupleTooLarge`を返す(`insert`の
    /// ドキュメントを参照)。
    pub fn update(
        &mut self,
        table_id: TableId,
        rid: RecordId,
        bytes: &[u8],
    ) -> DbResult<Option<RecordId>> {
        self.validate_rid(table_id, rid)?;

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
                let free = SlottedPage::open(guard.data_mut())?.free_space();
                drop(guard);
                self.fsm.update(rid.page_id, free);
                return Ok(Some(rid));
            }
            // このページの中には(コンパクションしても)収まらない。ここでは
            // まだ元の行を削除しない(このメソッドのドキュメントを参照)。
        }

        let new_rid = self.insert(table_id, bytes)?;
        match self.delete(table_id, rid) {
            Ok(true) => Ok(Some(new_rid)),
            Ok(false) => {
                // 直前にoccupiedを確認済みで、この章はシングルスレッド前提
                // なので通常は起こらない。万一起きた場合は、すでに書き込んだ
                // 新しい行をロールバックしてから異常として報告する。
                let _ = self.delete(table_id, new_rid);
                Err(DbError::CorruptPage(format!(
                    "update: 元のRecordId({rid:?})の削除に失敗しました(想定外)"
                )))
            }
            Err(err) => {
                let _ = self.delete(table_id, new_rid);
                Err(err)
            }
        }
    }

    /// `rid`が指すタプルを削除する。削除できたら`true`、対象がすでに存在しない
    /// (未挿入、または削除済み)なら`false`を返す。
    pub fn delete(&mut self, table_id: TableId, rid: RecordId) -> DbResult<bool> {
        self.validate_rid(table_id, rid)?;
        let mut guard = self.pool.write_page(rid.page_id)?;
        let deleted = SlottedPage::open(guard.data_mut())?.delete(rid.slot_id);
        if deleted {
            let free = SlottedPage::open(guard.data_mut())?.free_space();
            drop(guard);
            self.fsm.update(rid.page_id, free);
        }
        Ok(deleted)
    }

    /// `rid.page_id`が`table_id`のテーブルが所有するページであり、かつ
    /// `PageType::Data`であることを検証する。
    ///
    /// `get`・`update`・`delete`はすべて`rid.page_id`をそのままバッファプールへ
    /// 渡す前にこのチェックを通す。検証がなければ、別のテーブルの`RecordId`を
    /// 使い回して他テーブルの行を読む・書き換える・消す、あるいはMeta/Catalog
    /// ページの`PageId`を直接指定してその中身をタプルとして読み書きすることが
    /// できてしまう。前者は`TableEntry.page_ids`との突き合わせで、後者は
    /// `PageType`の確認で防ぐ。
    fn validate_rid(&self, table_id: TableId, rid: RecordId) -> DbResult<()> {
        let entry = self.table_entry(table_id)?;
        if !entry.page_ids.contains(&rid.page_id) {
            return Err(DbError::InvalidRecordId(format!(
                "PageId({})はTableId({})が所有するページではありません",
                rid.page_id.0, table_id.0
            )));
        }
        let guard = self.pool.read_page(rid.page_id)?;
        if guard.page_type() != PageType::Data {
            return Err(DbError::InvalidRecordId(format!(
                "PageId({})はPageType::Dataである必要がありますが{:?}でした",
                rid.page_id.0,
                guard.page_type()
            )));
        }
        Ok(())
    }

    /// `table_id`のテーブルの全ページを先頭から順に走査し、生きている全タプルを
    /// `(RecordId, タプルのバイト列)`として返すイテレータ。
    pub fn scan(&self, table_id: TableId) -> DbResult<Scan<'_>> {
        let entry = self.table_entry(table_id)?;
        Ok(Scan::new(&self.pool, &entry.page_ids))
    }

    fn table_entry(&self, table_id: TableId) -> DbResult<&TableEntry> {
        self.tables
            .get(&table_id)
            .ok_or_else(|| DbError::TableNotFound(format!("TableId({})", table_id.0)))
    }

    /// すでにデータが入っているかもしれないページへ、追記の形で挿入を試みる。
    /// 入らなければ`Ok(None)`を返す(呼び出し側が別の場所を探す)。
    fn try_insert_into_open_page(
        &mut self,
        page_id: PageId,
        bytes: &[u8],
    ) -> DbResult<Option<RecordId>> {
        let mut guard = self.pool.write_page(page_id)?;
        let slot = SlottedPage::open(guard.data_mut())?.insert(bytes);
        let free = SlottedPage::open(guard.data_mut())?.free_space();
        drop(guard);
        match slot {
            Some(slot) => {
                self.fsm.update(page_id, free);
                Ok(Some(RecordId::new(page_id, slot)))
            }
            None => Ok(None),
        }
    }

    /// `page_id`を`SlottedPage::init`で作り直してから挿入する。Free Page List
    /// から再利用したページにも、`allocate_page`で確保したばかりの新しいページ
    /// にも使う。
    fn try_insert_into_fresh_page(
        &mut self,
        page_id: PageId,
        bytes: &[u8],
    ) -> DbResult<Option<RecordId>> {
        let mut guard = self.pool.write_page(page_id)?;
        let slot = SlottedPage::init(guard.data_mut()).insert(bytes);
        let free = SlottedPage::open(guard.data_mut())?.free_space();
        drop(guard);
        match slot {
            Some(slot) => {
                self.fsm.update(page_id, free);
                Ok(Some(RecordId::new(page_id, slot)))
            }
            None => Ok(None),
        }
    }

    /// `page_id`を`table_id`の`page_ids`へ追加し、カタログを永続化し直す。
    /// 永続化に失敗した場合は追加を取り消してエラーを返す。
    fn attach_page_to_table(&mut self, table_id: TableId, page_id: PageId) -> DbResult<()> {
        self.tables
            .get_mut(&table_id)
            .expect("呼び出し元がtable_idの存在を確認済み")
            .page_ids
            .push(page_id);
        if let Err(err) = self.persist_catalog() {
            self.tables.get_mut(&table_id).unwrap().page_ids.pop();
            return Err(err);
        }
        Ok(())
    }

    /// 現在のテーブル定義・Free Page Listを`encode_catalog`でバイト列へ変換し、
    /// Catalogページへ書き込む。
    ///
    /// エンコード結果がCatalogページ1枚(`PAGE_PAYLOAD_SIZE`バイト)を超える場合は
    /// `DbError::CatalogTooLarge`を返す(モジュール冒頭の説明を参照)。
    fn persist_catalog(&self) -> DbResult<()> {
        let bytes = encode_catalog(self.next_table_id, &self.tables, &self.free_pages);
        if bytes.len() > PAGE_PAYLOAD_SIZE {
            return Err(DbError::CatalogTooLarge(bytes.len(), PAGE_PAYLOAD_SIZE));
        }
        let mut guard = self.pool.write_page(CATALOG_PAGE_ID)?;
        let data = guard.data_mut();
        data[..bytes.len()].copy_from_slice(&bytes);
        // 前回より短くなった分の末尾を0で埋め、古い内容の残骸を残さない。
        data[bytes.len()..].fill(0);
        Ok(())
    }
}

/// `decoded`のテーブル定義が意味的に矛盾していないかを検証する。
///
/// ページを読まずに済む(I/O不要の)検査だけをここへ集める。ページを読む必要が
/// ある検査(範囲・予約ページ・共有・`PageType`)は`Storage::open`側の
/// `claim_page`が担う。
///
/// - `next_table_id`が`u64::MAX`ではないこと。`u64::MAX`のままだと、次の
///   `create_table`が`TableId(self.next_table_id)`を払い出した直後の
///   `self.next_table_id += 1`でオーバーフローする(debugビルドではpanic、
///   releaseビルドでは0へ巻き戻って`TableId`の一意性が壊れる)。
/// - 各テーブルの`TableId`が`next_table_id`未満であること(そうでなければ、
///   次に`create_table`したテーブルが同じ`TableId`を再利用してしまう)。
/// - テーブル名が重複していないこと(`TableId`自体の重複は`decode_catalog`が
///   デコードの時点で検出済み)。
fn validate_table_metadata(decoded: &DecodedCatalog) -> DbResult<()> {
    if decoded.next_table_id == u64::MAX {
        return Err(DbError::CorruptCatalog(
            "next_table_idがu64::MAXです(これ以上TableIdを割り当てられません)".to_string(),
        ));
    }

    let mut seen_names = std::collections::HashSet::new();
    for entry in decoded.tables.values() {
        if entry.info.id.0 >= decoded.next_table_id {
            return Err(DbError::CorruptCatalog(format!(
                "TableId({})がnext_table_id({})以上です",
                entry.info.id.0, decoded.next_table_id
            )));
        }
        if !seen_names.insert(entry.info.name.as_str()) {
            return Err(DbError::CorruptCatalog(format!(
                "テーブル名'{}'が複数のTableIdに割り当てられています",
                entry.info.name
            )));
        }
    }
    Ok(())
}

/// `page_id`が有効な(範囲内かつ予約ページでない)データページであり、まだ
/// どのテーブル・Free Page Listにも属していないことを確認したうえで、
/// `claimed`へ登録する。
///
/// `page_id`がすでに`claimed`に含まれている場合は、あるページが複数の
/// テーブル(またはFree Page List)に同時に属していることになるため、
/// `DbError::CorruptCatalog`を返す。
fn claim_page(
    page_id: PageId,
    page_count: u64,
    claimed: &mut std::collections::HashSet<PageId>,
) -> DbResult<()> {
    if page_id.0 >= page_count {
        return Err(DbError::CorruptCatalog(format!(
            "PageId({})がページ数({page_count})の範囲外です",
            page_id.0
        )));
    }
    if page_id.0 <= CATALOG_PAGE_ID.0 {
        // PageId(0)はMetaページ、CATALOG_PAGE_ID(PageId(1))はCatalogページの
        // 定位置であり、どちらもテーブルのデータページにはなりえない。
        return Err(DbError::CorruptCatalog(format!(
            "PageId({})は予約ページ(Meta/Catalog)です",
            page_id.0
        )));
    }
    if !claimed.insert(page_id) {
        return Err(DbError::CorruptCatalog(format!(
            "PageId({})が複数のテーブル、またはFree Page Listと共有されています",
            page_id.0
        )));
    }
    Ok(())
}

/// `decode_catalog`が返す、Catalogページから復元した状態。
struct DecodedCatalog {
    next_table_id: u64,
    tables: HashMap<TableId, TableEntry>,
    free_pages: Vec<PageId>,
}

/// 現在のテーブル定義・Free Page Listをバイト列へエンコードする。
///
/// テーブルは`TableId`の昇順で書き出す。`tables`は`HashMap`であり反復順が
/// 実行のたびに変わりうるため、書き出す順序を固定しておかないと、論理的には
/// 同じ状態でもエンコード結果のバイト列が実行のたびに変わってしまう。
fn encode_catalog(
    next_table_id: u64,
    tables: &HashMap<TableId, TableEntry>,
    free_pages: &[PageId],
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&next_table_id.to_le_bytes());
    out.extend_from_slice(&(tables.len() as u32).to_le_bytes());
    out.extend_from_slice(&(free_pages.len() as u32).to_le_bytes());
    for &page_id in free_pages {
        out.extend_from_slice(&page_id.0.to_le_bytes());
    }

    let mut sorted: Vec<(&TableId, &TableEntry)> = tables.iter().collect();
    sorted.sort_by_key(|(id, _)| id.0);

    for (id, entry) in sorted {
        out.extend_from_slice(&id.0.to_le_bytes());
        let name_bytes = entry.info.name.as_bytes();
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(name_bytes);

        let columns = entry.info.schema.columns();
        out.extend_from_slice(&(columns.len() as u16).to_le_bytes());
        for column in columns {
            let col_name_bytes = column.name.as_bytes();
            out.extend_from_slice(&(col_name_bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(col_name_bytes);
            out.push(data_type_to_u8(column.data_type));
            out.push(u8::from(column.nullable));
        }

        out.extend_from_slice(&(entry.page_ids.len() as u32).to_le_bytes());
        for &page_id in &entry.page_ids {
            out.extend_from_slice(&page_id.0.to_le_bytes());
        }
    }

    out
}

/// Catalogページの`payload`から`DecodedCatalog`を復元する。
///
/// 宣言された`table_count`・`column_count`・`page_count`などの個数を
/// `Vec::with_capacity`の引数に直接使わない。バイト列が壊れていてこれらの
/// 個数が実際の残りバイト数よりずっと大きい場合でも、後続の`take`が
/// 都度その場で残りバイト数を検査するため、実際に確保するメモリ量は
/// 壊れたバイト列の長さそのもの(高々`PAGE_PAYLOAD_SIZE`)で頭打ちになる。
fn decode_catalog(bytes: &[u8]) -> DbResult<DecodedCatalog> {
    let mut cursor = bytes;

    let next_table_id = take_u64(&mut cursor, "next_table_id")?;
    let table_count = take_u32(&mut cursor, "table_count")? as usize;
    let free_page_count = take_u32(&mut cursor, "free_page_count")? as usize;

    let mut free_pages = Vec::new();
    for _ in 0..free_page_count {
        free_pages.push(PageId(take_u64(&mut cursor, "free_page_ids")?));
    }

    let mut tables = HashMap::new();
    for _ in 0..table_count {
        let table_id = TableId(take_u64(&mut cursor, "table_id")?);
        let name_len = take_u16(&mut cursor, "テーブル名の長さ")? as usize;
        let name = take_string(&mut cursor, name_len, "テーブル名")?;

        let column_count = take_u16(&mut cursor, "列数")? as usize;
        let mut columns = Vec::new();
        for _ in 0..column_count {
            let col_name_len = take_u16(&mut cursor, "列名の長さ")? as usize;
            let col_name = take_string(&mut cursor, col_name_len, "列名")?;
            let data_type = data_type_from_u8(take_u8(&mut cursor, "data_type")?)?;
            let nullable = match take_u8(&mut cursor, "nullable")? {
                0 => false,
                1 => true,
                other => {
                    return Err(DbError::CorruptCatalog(format!(
                        "nullableは0か1である必要がありますが{other}でした"
                    )));
                }
            };
            columns.push(Column::new(col_name, data_type, nullable));
        }

        let page_count = take_u32(&mut cursor, "page_count")? as usize;
        let mut page_ids = Vec::new();
        for _ in 0..page_count {
            page_ids.push(PageId(take_u64(&mut cursor, "page_ids")?));
        }

        let previous = tables.insert(
            table_id,
            TableEntry {
                info: TableInfo {
                    id: table_id,
                    name,
                    schema: Schema::new(columns),
                },
                page_ids,
            },
        );
        if previous.is_some() {
            // HashMapへそのままinsertすると後勝ちで上書きされ、重複が
            // 静かに消えてしまう。ここで検出しておかないと、同じTableIdを
            // 持つ2つのテーブル定義のうち片方が理由もなく失われる。
            return Err(DbError::CorruptCatalog(format!(
                "TableId({})が複数回出現しています",
                table_id.0
            )));
        }
    }

    Ok(DecodedCatalog {
        next_table_id,
        tables,
        free_pages,
    })
}

/// `*bytes`の先頭`n`バイトを切り出し、`*bytes`をその続きへ進める。
///
/// `*bytes`が`n`バイト未満しか残っていなければ`DbError::CorruptCatalog`を返す。
fn take<'a>(bytes: &mut &'a [u8], n: usize, what: &str) -> DbResult<&'a [u8]> {
    if bytes.len() < n {
        return Err(DbError::CorruptCatalog(format!(
            "{what}を読む前にバイト列が尽きました: {n}バイトが必要ですが{}バイトしかありません",
            bytes.len()
        )));
    }
    let (head, tail) = bytes.split_at(n);
    *bytes = tail;
    Ok(head)
}

fn take_u8(bytes: &mut &[u8], what: &str) -> DbResult<u8> {
    Ok(take(bytes, 1, what)?[0])
}

fn take_u16(bytes: &mut &[u8], what: &str) -> DbResult<u16> {
    Ok(u16::from_le_bytes(take(bytes, 2, what)?.try_into().unwrap()))
}

fn take_u32(bytes: &mut &[u8], what: &str) -> DbResult<u32> {
    Ok(u32::from_le_bytes(take(bytes, 4, what)?.try_into().unwrap()))
}

fn take_u64(bytes: &mut &[u8], what: &str) -> DbResult<u64> {
    Ok(u64::from_le_bytes(take(bytes, 8, what)?.try_into().unwrap()))
}

fn take_string(bytes: &mut &[u8], len: usize, what: &str) -> DbResult<String> {
    let raw = take(bytes, len, what)?;
    String::from_utf8(raw.to_vec())
        .map_err(|_| DbError::CorruptCatalog(format!("{what}が妥当なUTF-8ではありません")))
}

fn data_type_to_u8(data_type: DataType) -> u8 {
    match data_type {
        DataType::Boolean => 0,
        DataType::BigInt => 1,
        DataType::Text => 2,
    }
}

fn data_type_from_u8(byte: u8) -> DbResult<DataType> {
    match byte {
        0 => Ok(DataType::Boolean),
        1 => Ok(DataType::BigInt),
        2 => Ok(DataType::Text),
        other => Err(DbError::CorruptCatalog(format!(
            "未知のDataTypeコードです: {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{PAGE_SIZE, Page};
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};

    /// `Storage`は`Debug`を実装していない(`BufferPool`が実装していないため)。
    /// `unwrap_err`は`Ok`側の型に`Debug`を要求するため、代わりにこの小さな
    /// ヘルパーで`Result<Storage, DbError>`から`DbError`だけを取り出す。
    fn expect_err<T>(result: DbResult<T>) -> DbError {
        match result {
            Ok(_) => panic!("エラーを期待しましたが成功しました"),
            Err(err) => err,
        }
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-storage-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        path.push(unique);
        path
    }

    fn users_schema() -> Schema {
        Schema::new(vec![
            Column::new("id", DataType::BigInt, false),
            Column::new("name", DataType::Text, true),
        ])
    }

    #[test]
    fn create_then_get_round_trips_a_row() {
        let path = temp_path("create-get");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();

        let rid = storage.insert(table_id, b"alice").unwrap();
        assert_eq!(storage.get(table_id, rid).unwrap(), Some(b"alice".to_vec()));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn create_rejects_an_already_initialized_file() {
        let path = temp_path("create-twice");
        Storage::create(&path).unwrap();
        let err = expect_err(Storage::create(&path));
        assert!(matches!(err, DbError::CorruptPage(_)));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_file_without_a_catalog_page() {
        // 素のDiskManagerだけで作った(ページ0しか無い)ファイルは、
        // Storage::createを経由していないのでCatalogページを持たない。
        let path = temp_path("open-no-catalog");
        DiskManager::open(&path).unwrap();
        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptPage(_)));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_bad_magic_number() {
        // DiskManager::openのFile Header検証(第13章)が、Storage::open経由でも
        // そのまま効くことを確認する。ページ自体のchecksumは正しく計算し直した
        // うえで、Magic Numberの先頭バイトだけを壊す。
        let path = temp_path("open-bad-magic");
        Storage::create(&path).unwrap();

        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let mut page_bytes = [0u8; PAGE_SIZE];
        file.seek(SeekFrom::Start(0)).unwrap();
        file.read_exact(&mut page_bytes).unwrap();
        let mut page = Page::decode(&page_bytes).unwrap();
        page.payload_mut()[0] = b'X'; // FileHeaderのMagic Numberの先頭バイトを壊す。
        let bytes = page.encode(); // Pageレベルのchecksumは正しく計算し直される。

        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(&bytes).unwrap();

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptPage(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn corrupting_a_byte_in_the_catalog_page_is_detected_on_open() {
        // 「壊して確認する」実験その1: Catalogページのバイト列を1つ反転させると、
        // Page::decodeのchecksum検証(第11章)がそのまま効いてopenが失敗する。
        let path = temp_path("corrupt-catalog-checksum");
        {
            let mut storage = Storage::create(&path).unwrap();
            let table_id = storage.create_table("users", users_schema()).unwrap();
            storage.insert(table_id, b"alice").unwrap();
            storage.flush().unwrap();
        }

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        // Catalogページ(PageId(1))はファイルの2区画目にある。
        file.seek(SeekFrom::Start(PAGE_SIZE as u64 + 40)).unwrap();
        file.write_all(&[0xFF]).unwrap();

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptPage(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_structurally_valid_but_nonsensical_catalog_is_rejected_as_corrupt() {
        // 「壊して確認する」実験その2: checksumは正しい(壊れたページとしては
        // 検出されない)が、中身の`name_len`が実際に残っているバイト数と
        // 矛盾しているCatalogページを直接作り、decode_catalogの境界検査で
        // 捕まることを確認する。
        let path = temp_path("corrupt-catalog-content");
        Storage::create(&path).unwrap();

        let mut payload = vec![0u8; PAGE_PAYLOAD_SIZE];
        payload[0..8].copy_from_slice(&0u64.to_le_bytes()); // next_table_id
        payload[8..12].copy_from_slice(&1u32.to_le_bytes()); // table_count = 1
        payload[12..16].copy_from_slice(&0u32.to_le_bytes()); // free_page_count
        payload[16..24].copy_from_slice(&0u64.to_le_bytes()); // table_id
        // name_lenを、ページに残っている実バイト数よりずっと大きい値へ偽る。
        payload[24..26].copy_from_slice(&u16::MAX.to_le_bytes());

        let mut page = Page::new(CATALOG_PAGE_ID, PageType::Catalog);
        page.payload_mut().copy_from_slice(&payload);
        let bytes = page.encode(); // checksumは正しく計算される。

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        file.write_all(&bytes).unwrap();

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    /// `bytes`(`encode_catalog`の出力、または手書きのバイト列)をCatalogページの
    /// `payload`としてそのまま`path`へ書き込む。checksumは`Page::encode`が
    /// 正しく計算し直すため、以下の意味検証テストはどれも「構造としては
    /// 正しく読めるが、中身が意味をなさない」状態を作る。
    fn write_catalog_payload(path: &std::path::Path, bytes: &[u8]) {
        let mut payload = vec![0u8; PAGE_PAYLOAD_SIZE];
        payload[..bytes.len()].copy_from_slice(bytes);

        let mut page = Page::new(CATALOG_PAGE_ID, PageType::Catalog);
        page.payload_mut().copy_from_slice(&payload);
        let encoded = page.encode();

        let mut file = OpenOptions::new().write(true).open(path).unwrap();
        file.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        file.write_all(&encoded).unwrap();
    }

    fn single_table_entry(id: TableId, name: &str, page_ids: Vec<PageId>) -> HashMap<TableId, TableEntry> {
        let mut tables = HashMap::new();
        tables.insert(
            id,
            TableEntry {
                info: TableInfo {
                    id,
                    name: name.to_string(),
                    schema: users_schema(),
                },
                page_ids,
            },
        );
        tables
    }

    #[test]
    fn open_rejects_free_pages_that_reference_the_meta_page() {
        // 再現ケース: free_pagesにMetaページ(PageId(0))が紛れ込んだカタログは
        // checksumも構造も正しく読めてしまう。意味検証がなければこれはopenに
        // 成功し、次のinsertがFree Page Listから0を取り出してMetaページを
        // 「空きページ」として上書きし、以後そのファイルをopenできなくなる。
        let path = temp_path("free-pages-include-meta");
        Storage::create(&path).unwrap();

        let tables = single_table_entry(TableId(0), "a", Vec::new());
        let bytes = encode_catalog(1, &tables, &[PageId(0)]);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_table_page_id_out_of_range() {
        let path = temp_path("page-id-out-of-range");
        Storage::create(&path).unwrap();

        let tables = single_table_entry(TableId(0), "a", vec![PageId(999)]);
        let bytes = encode_catalog(1, &tables, &[]);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_table_page_that_is_not_page_type_data() {
        let path = temp_path("table-page-not-data");
        {
            let disk = DiskManager::open(&path).unwrap();
            let pool = BufferPool::new(disk, 8);
            let catalog_page_id = pool.allocate_page(PageType::Catalog).unwrap();
            assert_eq!(catalog_page_id, CATALOG_PAGE_ID);
            // 本来テーブルのデータページに使わないPageType(ここでは2枚目の
            // Catalogページ)を、テーブルのpage_idsへ直接登録する。
            let bogus_data_page = pool.allocate_page(PageType::Catalog).unwrap();

            let tables = single_table_entry(TableId(0), "a", vec![bogus_data_page]);
            let bytes = encode_catalog(1, &tables, &[]);
            let mut guard = pool.write_page(CATALOG_PAGE_ID).unwrap();
            let data = guard.data_mut();
            data[..bytes.len()].copy_from_slice(&bytes);
            data[bytes.len()..].fill(0);
            drop(guard);
            pool.flush_all().unwrap();
        }

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_page_shared_between_two_tables() {
        let path = temp_path("shared-page");
        let (a, b, shared_page) = {
            let mut storage = Storage::create(&path).unwrap();
            let a = storage.create_table("a", users_schema()).unwrap();
            storage.insert(a, b"x").unwrap();
            let b = storage.create_table("b", users_schema()).unwrap();
            let shared_page = storage.tables.get(&a).unwrap().page_ids[0];
            storage.flush().unwrap();
            (a, b, shared_page)
        };

        // aが実際に使っているページを、bのpage_idsとしても登録する。
        let mut tables = single_table_entry(a, "a", vec![shared_page]);
        tables.insert(
            b,
            TableEntry {
                info: TableInfo {
                    id: b,
                    name: "b".to_string(),
                    schema: users_schema(),
                },
                page_ids: vec![shared_page],
            },
        );
        let bytes = encode_catalog(2, &tables, &[]);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_table_id_that_is_not_less_than_next_table_id() {
        let path = temp_path("table-id-not-less-than-next");
        Storage::create(&path).unwrap();

        // TableId(0)が存在するのにnext_table_idも0のまま、というカタログ。
        // 次のcreate_tableがTableId(0)を再利用してしまう矛盾がある。
        let tables = single_table_entry(TableId(0), "a", Vec::new());
        let bytes = encode_catalog(0, &tables, &[]);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_catalog_whose_next_table_id_is_u64_max() {
        // next_table_id=u64::MAX、table_count=0という、checksum・構造ともに
        // 正常なカタログ。これをそのまま受理すると、次のcreate_tableが
        // TableId(u64::MAX)を払い出した直後にnext_table_idへの加算で
        // オーバーフローする(debugビルドはpanic、releaseビルドは0へ巻き戻って
        // TableIdの一意性が壊れる)。
        let path = temp_path("next-table-id-u64-max");
        Storage::create(&path).unwrap();

        let tables: HashMap<TableId, TableEntry> = HashMap::new();
        let bytes = encode_catalog(u64::MAX, &tables, &[]);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn create_table_rejects_when_next_table_id_would_overflow() {
        // Storage::openの検証をすり抜けてメモリ上だけでnext_table_idが
        // u64::MAXに達した場合でも、create_table自身のchecked_addが
        // オーバーフローをTableIdSpaceExhaustedとして検出する(二重の備え)。
        let path = temp_path("create-table-overflow");
        let mut storage = Storage::create(&path).unwrap();
        storage.next_table_id = u64::MAX;

        let err = expect_err(storage.create_table("a", users_schema()));
        assert!(matches!(err, DbError::TableIdSpaceExhausted));
        // 失敗した場合、next_table_idもテーブル一覧も変化しない。
        assert_eq!(storage.next_table_id, u64::MAX);
        assert!(storage.tables.is_empty());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_duplicate_table_names() {
        let path = temp_path("duplicate-table-name");
        Storage::create(&path).unwrap();

        let mut tables = single_table_entry(TableId(0), "dup", Vec::new());
        tables.insert(
            TableId(1),
            TableEntry {
                info: TableInfo {
                    id: TableId(1),
                    name: "dup".to_string(),
                    schema: users_schema(),
                },
                page_ids: Vec::new(),
            },
        );
        let bytes = encode_catalog(2, &tables, &[]);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_catalog_with_a_duplicate_table_id() {
        // encode_catalogはHashMapを介するため、同じTableIdを2回持つカタログを
        // 正規の経路では作れない。decode_catalog自身の重複検出を確認するため、
        // ここだけはバイト列を手で組み立てる。
        let path = temp_path("duplicate-table-id");
        Storage::create(&path).unwrap();

        let mut payload = Vec::new();
        payload.extend_from_slice(&2u64.to_le_bytes()); // next_table_id
        payload.extend_from_slice(&2u32.to_le_bytes()); // table_count = 2
        payload.extend_from_slice(&0u32.to_le_bytes()); // free_page_count

        for name in ["a", "b"] {
            payload.extend_from_slice(&0u64.to_le_bytes()); // table_id (両方とも0)
            payload.extend_from_slice(&(name.len() as u16).to_le_bytes());
            payload.extend_from_slice(name.as_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes()); // column_count = 0
            payload.extend_from_slice(&0u32.to_le_bytes()); // page_count = 0
        }

        write_catalog_payload(&path, &payload);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopening_preserves_tables_and_rows() {
        let path = temp_path("reopen");
        {
            let mut storage = Storage::create(&path).unwrap();
            let users = storage.create_table("users", users_schema()).unwrap();
            let posts = storage
                .create_table(
                    "posts",
                    Schema::new(vec![Column::new("title", DataType::Text, false)]),
                )
                .unwrap();
            storage.insert(users, b"alice").unwrap();
            storage.insert(users, b"bob").unwrap();
            storage.insert(posts, b"hello world").unwrap();
            storage.flush().unwrap();
        }

        let storage = Storage::open(&path).unwrap();
        let users_info = storage.table("users").unwrap();
        assert_eq!(users_info.schema, users_schema());
        let posts_info = storage.table("posts").unwrap();

        let users_rows: Vec<_> = storage
            .scan(users_info.id)
            .unwrap()
            .collect::<DbResult<Vec<_>>>()
            .unwrap();
        assert_eq!(users_rows.len(), 2);

        let posts_rows: Vec<_> = storage
            .scan(posts_info.id)
            .unwrap()
            .collect::<DbResult<Vec<_>>>()
            .unwrap();
        assert_eq!(posts_rows.len(), 1);
        assert_eq!(posts_rows[0].1, b"hello world".to_vec());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn two_tables_share_one_file_and_interleave_pages() {
        let path = temp_path("two-tables");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();
        let b = storage.create_table("b", users_schema()).unwrap();

        storage.insert(a, b"a-row").unwrap();
        storage.insert(b, b"b-row").unwrap();

        let a_pages = storage.tables.get(&a).unwrap().page_ids.clone();
        let b_pages = storage.tables.get(&b).unwrap().page_ids.clone();
        assert_eq!(a_pages.len(), 1);
        assert_eq!(b_pages.len(), 1);
        // 2つのテーブルは同じファイルの別々のページを使っており、重複しない。
        assert_ne!(a_pages[0], b_pages[0]);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn dropping_a_table_frees_its_pages_for_reuse() {
        let path = temp_path("drop-reuse");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();
        // 大きめの1件で1ページ目を専有させる。
        storage.insert(a, &vec![b'x'; 3000]).unwrap();
        let a_first_page = storage.tables.get(&a).unwrap().page_ids[0];

        storage.drop_table("a").unwrap();
        assert_eq!(storage.free_pages, vec![a_first_page]);

        let b = storage.create_table("b", users_schema()).unwrap();
        storage.insert(b, &vec![b'y'; 3000]).unwrap();
        let b_first_page = storage.tables.get(&b).unwrap().page_ids[0];

        // dropしたテーブルのページがそのまま再利用されている。
        assert_eq!(b_first_page, a_first_page);
        assert!(storage.free_pages.is_empty());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn insert_skips_full_pages_and_lands_on_a_page_with_real_room() {
        let path = temp_path("fsm-skip-full-pages");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();

        // 3000バイトの行を2件入れると、どちらも別のページを1枚ずつ専有し、
        // どちらのページにも(コンパクションなしでは)大きな空きが残らない。
        let big = vec![b'x'; 3000];
        let rid1 = storage.insert(a, &big).unwrap();
        let rid2 = storage.insert(a, &big).unwrap();
        assert_ne!(rid1.page_id, rid2.page_id);

        // 1200バイトの行は、既存の2ページのどちらの残り空きにも収まらないため、
        // Free Space Mapが両方を候補から外し、3ページ目が新たに確保される。
        let rid3 = storage.insert(a, &vec![b'y'; 1200]).unwrap();
        assert_ne!(rid3.page_id, rid1.page_id);
        assert_ne!(rid3.page_id, rid2.page_id);
        assert_eq!(storage.tables.get(&a).unwrap().page_ids.len(), 3);

        // 900バイトの行は1ページ目の残り空きに収まるため、Free Space Mapが
        // 1ページ目を候補として見つけ、新しいページを増やさずにそこへ入る。
        let rid4 = storage.insert(a, &vec![b'z'; 900]).unwrap();
        assert_eq!(rid4.page_id, rid1.page_id);
        assert_eq!(storage.tables.get(&a).unwrap().page_ids.len(), 3);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn catalog_too_large_is_rejected_instead_of_corrupting_the_file() {
        let path = temp_path("catalog-too-large");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();

        // 1ページに1件しか入らない大きさの行を、カタログがCatalogページに
        // 収まらなくなるまで挿入し続ける。
        let big = vec![b'x'; 3000];
        let mut hit_limit = false;
        for _ in 0..2000 {
            match storage.insert(a, &big) {
                Ok(_) => {}
                Err(DbError::CatalogTooLarge(needed, capacity)) => {
                    assert!(needed > capacity);
                    hit_limit = true;
                    break;
                }
                Err(other) => panic!("CatalogTooLargeを期待しましたが別のエラーでした: {other}"),
            }
        }
        assert!(hit_limit, "2000件挿入してもCatalogTooLargeにならなかった");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn create_table_rejects_duplicate_name() {
        let path = temp_path("dup-table");
        let mut storage = Storage::create(&path).unwrap();
        storage.create_table("users", users_schema()).unwrap();
        let result = storage.create_table("users", users_schema());
        assert!(matches!(result, Err(DbError::DuplicateTable(name)) if name == "users"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn drop_table_rejects_unknown_name() {
        let path = temp_path("drop-unknown");
        let mut storage = Storage::create(&path).unwrap();
        let result = storage.drop_table("users");
        assert!(matches!(result, Err(DbError::TableNotFound(name)) if name == "users"));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn table_ids_are_not_reused_after_drop_even_across_reopen() {
        let path = temp_path("id-not-reused");
        {
            let mut storage = Storage::create(&path).unwrap();
            let first = storage.create_table("users", users_schema()).unwrap();
            storage.drop_table("users").unwrap();
            assert_eq!(first, TableId(0));
            storage.flush().unwrap();
        }

        let mut storage = Storage::open(&path).unwrap();
        let second = storage.create_table("users", users_schema()).unwrap();
        // 再オープンをまたいでも、削除済みのTableId(0)は再利用されない。
        assert_eq!(second, TableId(1));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn update_delete_and_scan_behave_like_heap_file() {
        let path = temp_path("crud");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();

        let rid = storage.insert(a, b"aaaaa").unwrap();
        let new_rid = storage.update(a, rid, b"bbbbb").unwrap().unwrap();
        assert_eq!(new_rid, rid);
        assert_eq!(storage.get(a, rid).unwrap(), Some(b"bbbbb".to_vec()));

        assert!(storage.delete(a, rid).unwrap());
        assert_eq!(storage.get(a, rid).unwrap(), None);
        assert_eq!(storage.scan(a).unwrap().count(), 0);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn update_that_fails_to_insert_leaves_the_original_row_intact() {
        let path = temp_path("update-insert-fails");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();

        let rid = storage.insert(a, b"original").unwrap();
        storage.flush().unwrap();
        let page_ids_before = storage.tables.get(&a).unwrap().page_ids.clone();
        let page_count_before = storage.pool.page_count();
        let file_size_before = std::fs::metadata(&path).unwrap().len();

        // 空の1ページにも収まらないほど大きい値へのupdateは、まず新しい場所への
        // insertを試み、それがTupleTooLargeで失敗する。旧行を先に消していれば
        // この時点でデータが失われるが、insertを先に試す実装ではrid経由の
        // 元の行がそのまま読める。
        let too_big = vec![b'x'; PAGE_PAYLOAD_SIZE + 1];
        let err = expect_err(storage.update(a, rid, &too_big));
        assert!(matches!(err, DbError::TupleTooLarge(_)));
        storage.flush().unwrap();

        assert_eq!(storage.get(a, rid).unwrap(), Some(b"original".to_vec()));
        // 失敗したupdateは、insert前の事前検査で弾かれるべきであり、新しい
        // ページを確保してはならない(page_ids・page_count・ファイルサイズが
        // すべて変わらない)。
        assert_eq!(storage.tables.get(&a).unwrap().page_ids, page_ids_before);
        assert_eq!(storage.pool.page_count(), page_count_before);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), file_size_before);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn insert_that_is_too_large_does_not_grow_the_file() {
        let path = temp_path("insert-too-large-no-growth");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();
        let rid = storage.insert(a, b"small").unwrap();
        storage.flush().unwrap();

        let page_ids_before = storage.tables.get(&a).unwrap().page_ids.clone();
        let page_count_before = storage.pool.page_count();
        let file_size_before = std::fs::metadata(&path).unwrap().len();

        // 空の1ページにも収まらないほど大きいinsertを繰り返しても、新しい
        // ページを確保してはならない。事前検査が無いと、失敗するinsertの
        // たびにFree Page Listの消費やallocate_pageでファイルが肥大化する。
        let too_big = vec![b'x'; PAGE_PAYLOAD_SIZE + 1];
        for _ in 0..3 {
            let err = expect_err(storage.insert(a, &too_big));
            assert!(matches!(err, DbError::TupleTooLarge(_)));
        }
        storage.flush().unwrap();

        assert_eq!(storage.tables.get(&a).unwrap().page_ids, page_ids_before);
        assert_eq!(storage.pool.page_count(), page_count_before);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), file_size_before);
        assert!(storage.free_pages.is_empty());
        assert_eq!(storage.get(a, rid).unwrap(), Some(b"small".to_vec()));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_too_large_insert_does_not_leak_a_page_popped_from_the_free_page_list() {
        // dropしたテーブルが残したFree Page Listのページを、失敗する
        // insertが取り出したきり戻さずに宙へ浮かせないことを確認する。
        let path = temp_path("free-page-not-leaked");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();
        storage.insert(a, &vec![b'x'; 3000]).unwrap();
        storage.drop_table("a").unwrap();
        assert_eq!(storage.free_pages.len(), 1);
        let freed_page = storage.free_pages[0];

        let b = storage.create_table("b", users_schema()).unwrap();
        let too_big = vec![b'y'; PAGE_PAYLOAD_SIZE + 1];
        let err = expect_err(storage.insert(b, &too_big));
        assert!(matches!(err, DbError::TupleTooLarge(_)));

        // Free Page Listのページは、失敗したinsertに取られたままにならず
        // そのまま残っている。
        assert_eq!(storage.free_pages, vec![freed_page]);
        assert!(storage.tables.get(&b).unwrap().page_ids.is_empty());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_5013_byte_update_does_not_grow_the_file_when_it_cannot_fit_anywhere() {
        // レビューで再現された具体的なケース: 5013バイトのUPDATEが失敗しても
        // ファイルサイズが増えないことを、ページサイズの単位(4096バイト)で
        // 直接確認する。
        let path = temp_path("5013-byte-update-no-growth");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();
        let rid = storage.insert(a, b"x").unwrap();
        storage.flush().unwrap();
        let file_size_before = std::fs::metadata(&path).unwrap().len();

        let too_big = vec![b'z'; 5013];
        assert!(too_big.len() > PAGE_PAYLOAD_SIZE);
        let err = expect_err(storage.update(a, rid, &too_big));
        assert!(matches!(err, DbError::TupleTooLarge(_)));
        storage.flush().unwrap();

        assert_eq!(std::fs::metadata(&path).unwrap().len(), file_size_before);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn get_update_delete_reject_a_record_id_from_another_table() {
        let path = temp_path("cross-table-rid");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();
        let b = storage.create_table("b", users_schema()).unwrap();

        let rid_in_a = storage.insert(a, b"a-row").unwrap();
        // rid_in_aのpage_idはテーブルaのものだが、bのTableIdで参照する。
        assert!(matches!(
            storage.get(b, rid_in_a),
            Err(DbError::InvalidRecordId(_))
        ));
        assert!(matches!(
            storage.update(b, rid_in_a, b"x"),
            Err(DbError::InvalidRecordId(_))
        ));
        assert!(matches!(
            storage.delete(b, rid_in_a),
            Err(DbError::InvalidRecordId(_))
        ));

        // aからは正しく読めたままである(bからの誤った操作の影響を受けていない)。
        assert_eq!(storage.get(a, rid_in_a).unwrap(), Some(b"a-row".to_vec()));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn get_update_delete_reject_a_record_id_pointing_at_the_catalog_page() {
        let path = temp_path("catalog-rid");
        let mut storage = Storage::create(&path).unwrap();
        let a = storage.create_table("a", users_schema()).unwrap();

        // Catalogページ(PageId(1))を指す、テーブルaには属さないRecordIdを
        // 直接組み立てる。
        let bogus_rid = RecordId::new(CATALOG_PAGE_ID, crate::ids::SlotId(0));
        assert!(matches!(
            storage.get(a, bogus_rid),
            Err(DbError::InvalidRecordId(_))
        ));
        assert!(matches!(
            storage.update(a, bogus_rid, b"x"),
            Err(DbError::InvalidRecordId(_))
        ));
        assert!(matches!(
            storage.delete(a, bogus_rid),
            Err(DbError::InvalidRecordId(_))
        ));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn operations_on_unknown_table_id_are_rejected() {
        let path = temp_path("unknown-table-id");
        let mut storage = Storage::create(&path).unwrap();
        let bogus = TableId(999);
        assert!(matches!(
            storage.insert(bogus, b"x"),
            Err(DbError::TableNotFound(_))
        ));
        assert!(matches!(storage.scan(bogus), Err(DbError::TableNotFound(_))));
        std::fs::remove_file(&path).unwrap();
    }
}
