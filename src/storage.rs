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
use crate::slotted_page::{SlotStatus, SlottedPage, SlottedPageRef};
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
    /// `DbError::CorruptPage`、カタログのバイト列自体は読めるが内容が矛盾している
    /// 場合は`DbError::CorruptCatalog`を返す。
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

        // Free Space Mapは永続化しない(モジュール冒頭の説明を参照)。
        // カタログから復元した各テーブルのpage_idsを1回ずつ読み、実測の
        // free_space()から作り直す。
        let mut fsm = FreeSpaceMap::new();
        for entry in decoded.tables.values() {
            for &page_id in &entry.page_ids {
                let guard = pool.read_page(page_id)?;
                let free = SlottedPageRef::open(guard.data()).free_space();
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
    /// 呼ぶだけの薄いラッパーである。`DiskManager::sync`までは行わないため、
    /// プロセスの再起動をまたいでデータを残したい呼び出し側は、この後で
    /// 別途`sync`が必要になる場面がありうる。
    pub fn flush(&self) -> DbResult<()> {
        self.pool.flush_all()
    }

    /// テーブル名から`TableInfo`を引く。見つからなければ`None`を返す。
    pub fn table(&self, name: &str) -> Option<&TableInfo> {
        self.tables.values().find(|t| t.info.name == name).map(|t| &t.info)
    }

    /// 新しいテーブルを登録する。
    ///
    /// 同名のテーブルがすでに存在する場合は`DbError::DuplicateTable`を返す。
    /// カタログの永続化(`persist_catalog`)に失敗した場合(たとえば
    /// `DbError::CatalogTooLarge`)は、メモリ上の登録も取り消す。カタログに
    /// 書き出せていないテーブルをメモリ上にだけ存在させておくと、次の操作で
    /// メモリとディスクの内容が食い違ってしまう。
    pub fn create_table(&mut self, name: &str, schema: Schema) -> DbResult<TableId> {
        if self.tables.values().any(|t| t.info.name == name) {
            return Err(DbError::DuplicateTable(name.to_string()));
        }

        let id = TableId(self.next_table_id);
        self.next_table_id += 1;
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
            self.next_table_id -= 1;
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
    pub fn insert(&mut self, table_id: TableId, bytes: &[u8]) -> DbResult<RecordId> {
        let needed = bytes.len();
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
                None => return Err(DbError::TupleTooLarge(needed)),
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
        self.table_entry(table_id)?;
        let guard = self.pool.read_page(rid.page_id)?;
        Ok(SlottedPageRef::open(guard.data())
            .get(rid.slot_id)
            .map(|bytes| bytes.to_vec()))
    }

    /// `rid`が指すタプルを`bytes`へ置き換える。
    ///
    /// `HeapFile::update`(第13章)と同じく、同じページに(コンパクション後も)
    /// 収まる限り同じ`RecordId`を保つ。収まらない場合は、このページからは
    /// 削除し、`insert`と同じ経路(Free Space Map→Free Page List→新規ページ)で
    /// 別の場所へ挿入し直す。対象が存在しなければ`Ok(None)`を返す。
    pub fn update(
        &mut self,
        table_id: TableId,
        rid: RecordId,
        bytes: &[u8],
    ) -> DbResult<Option<RecordId>> {
        self.table_entry(table_id)?;

        let occupied = {
            let guard = self.pool.read_page(rid.page_id)?;
            SlottedPageRef::open(guard.data()).status(rid.slot_id) == Some(SlotStatus::Occupied)
        };
        if !occupied {
            return Ok(None);
        }

        {
            let mut guard = self.pool.write_page(rid.page_id)?;
            if SlottedPage::open(guard.data_mut()).update(rid.slot_id, bytes) {
                let free = SlottedPage::open(guard.data_mut()).free_space();
                drop(guard);
                self.fsm.update(rid.page_id, free);
                return Ok(Some(rid));
            }
            // このページの中には(コンパクションしても)収まらないので、
            // このページからは削除し、別のページへ挿入し直す。
            SlottedPage::open(guard.data_mut()).delete(rid.slot_id);
            let free = SlottedPage::open(guard.data_mut()).free_space();
            drop(guard);
            self.fsm.update(rid.page_id, free);
        }
        let new_rid = self.insert(table_id, bytes)?;
        Ok(Some(new_rid))
    }

    /// `rid`が指すタプルを削除する。削除できたら`true`、対象がすでに存在しない
    /// (未挿入、または削除済み)なら`false`を返す。
    pub fn delete(&mut self, table_id: TableId, rid: RecordId) -> DbResult<bool> {
        self.table_entry(table_id)?;
        let mut guard = self.pool.write_page(rid.page_id)?;
        let deleted = SlottedPage::open(guard.data_mut()).delete(rid.slot_id);
        if deleted {
            let free = SlottedPage::open(guard.data_mut()).free_space();
            drop(guard);
            self.fsm.update(rid.page_id, free);
        }
        Ok(deleted)
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
        let slot = SlottedPage::open(guard.data_mut()).insert(bytes);
        let free = SlottedPage::open(guard.data_mut()).free_space();
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
        let free = SlottedPage::open(guard.data_mut()).free_space();
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

        tables.insert(
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
