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
//!         col_name_len:  u16
//!         col_name:      u8 × col_name_len
//!         data_type:     u8 (0=BOOLEAN, 1=BIGINT, 2=TEXT)
//!         nullable:      u8 (0 または 1)
//!         primary_key:   u8 (0 または 1) ※第20章で追加
//!         unique:        u8 (0 または 1) ※第20章で追加
//!     page_count:     u32
//!     page_ids:       u64 × page_count
//! ```
//!
//! # 第20章での変更: 列ごとの制約バイトを追加する
//!
//! `PRIMARY KEY`・`UNIQUE`(第20章)は`Column`に持たせる情報が2つ増えたため、
//! 列ごとのレコードの末尾(`nullable`の直後)に`primary_key`・`unique`という
//! 2バイトを追加した。この章より前に`Storage::create`で作られたファイルは
//! この2バイトを持たないため、この章のコードで`Storage::open`しようとすると
//! `nullable`の直後で次の列(または`page_count`)を読もうとして境界がずれ、
//! `DbError::CorruptCatalog`になる。
//!
//! ページ構造そのもの(File Header・Pageの`checksum`・`FORMAT_VERSION`、
//! [`crate::page`])はこの章でも変えていない。`Page::decode`が検証する
//! `FORMAT_VERSION`は「ページというバイト列の外枠(ヘッダ・checksum・
//! `payload`のサイズ)が読めるか」だけを保証する番号であり、Catalogページの
//! `payload`の中身(このモジュールが独自に手書きしているバイナリレイアウト)
//! までは関知しない。したがって、`payload`内のレイアウトを変えるたびに
//! `FORMAT_VERSION`を上げる方針は採らない。採ってしまうと、`payload`の中身に
//! 一切関心のない`page`モジュールが、他のモジュール(このモジュールや、将来
//! 増えるページ種別)の内部レイアウト変更のたびに変更を強いられることになる。
//!
//! 代わりに、この教材はそもそも「異なる章のコードでビルドしたデータベース
//! ファイル間の互換性」を約束していない。各章は`git`タグで区切られた
//! 1つのスナップショットであり、`Storage::open`が読めるのは同じ章の
//! `Storage::create`(または、レイアウトを変えていない章)が書いたファイルに
//! 限られる。この章のように`payload`のレイアウトを変える場合は、モジュール
//! 冒頭のコメント(このセクション)へ変更内容を書き残すことで、読者が
//! 「なぜ前の章で作ったファイルをこの章のコードで開けなくなったか」を
//! たどれるようにする。これは新しい方針ではなく、第15章でこのモジュールが
//! 生まれたときから変わっていない前提を、初めて実際に変更が起きたこの章で
//! 明文化しただけである。
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
//!
//! # 第24章での変更: 索引メタデータをCatalogページへ追加する
//!
//! `CREATE INDEX`(第24章)が作る索引の一覧を、テーブル定義と同じCatalogページ
//! へ追記する。列ごとの制約バイト(第20章)と同じ考え方で、既存のレイアウトの
//! 末尾に新しいセクションを足すだけであり、既存のフィールドは1つも動かさない。
//!
//! ```text
//! index_count: u32
//! indexes × index_count:
//!     name_len:        u16
//!     name:            u8 × name_len
//!     table_id:        u64
//!     column_index:    u16
//!     column_name_len: u16
//!     column_name:     u8 × column_name_len
//!     unique:          u8 (0 または 1)
//!     primary_key:     u8 (0 または 1)
//!     key_type:        u8 (0=BOOLEAN, 1=BIGINT, 2=TEXT)
//! ```
//!
//! この章より前(第23章以前)に`Storage::create`で作られたファイルはこの
//! セクションを持たないため、この章のコードで開こうとすると`index_count`を
//! 読む前にバイト列が尽き、`DbError::CorruptCatalog`になる(モジュール冒頭の
//! 「第20章での変更」節と同じ、章をまたいだファイル互換性を約束しない方針)。
//!
//! 索引の実データ(`crate::btree::BTree`が持つB+Tree本体)は、この
//! Catalogページと同じファイルには置かない。**索引ごとに専用のファイル**
//! (`<データベースファイルのパス>.idx.<索引名>`)を持たせ、その中では
//! `crate::btree::BTree`が第23章から変わらない前提(Metaページはページ1)で
//! 動く。この設計を選んだ理由は、`crate::btree::BTree`のMetaページが
//! ページ1固定という前提(第23章)を、複数の索引を同じファイルに同居させる
//! ために書き換えずに済むからである。テーブルのデータページを1つのファイルへ
//! まとめた`Storage`自身の設計(モジュール冒頭)とは対照的だが、`HeapFile`
//! (第13章)がテーブルごとに専用ファイルを持っていた設計をそのまま索引にも
//! 転用したと捉えられる。したがって、この節の`index_count`のセクションが
//! 持つのは索引の**メタデータ**(名前・テーブル・列・`unique`・キー型)だけで、
//! B+Treeの`Root`の`PageId`はここには現れない(索引ごとのファイルの中で
//! `BTree`自身が管理する)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::btree::BTree;
use crate::buffer_pool::BufferPool;
use crate::catalog::TableInfo;
use crate::disk_manager::DiskManager;
use crate::error::{DbError, DbResult};
use crate::free_space_map::FreeSpaceMap;
use crate::heap_file::Scan;
use crate::ids::{PageId, RecordId, TableId};
use crate::index::IndexInfo;
use crate::page::{PAGE_PAYLOAD_SIZE, PageType};
use crate::slotted_page::{SlotStatus, SlottedPage, SlottedPageRef, max_len_for_fresh_page};
use crate::tuple_codec::decode_tuple;
use crate::types::{Column, DataType, Schema, Tuple};

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

/// カタログに登録された1つの索引の定義と、その実データ(`BTree`)の組
/// (第24章)。`crate::index::IndexInfo`はメタデータだけの値型で、`BTree`本体は
/// `Storage`だけが所有する(モジュール冒頭の「索引ごとに専用のファイル」を参照)。
struct IndexEntry {
    info: IndexInfo,
    btree: BTree,
}

/// `db_path`の索引`index_name`が使う専用ファイルのパスを組み立てる
/// (第24章)。`<db_path>.idx.<index_name>`という命名で、`db_path`本体
/// (テーブル定義・データページ)とは別のファイルにする(モジュール冒頭を参照)。
fn index_file_path(db_path: &Path, index_name: &str) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".idx.");
    os_string.push(index_name);
    PathBuf::from(os_string)
}

/// テーブル定義とデータページの両方を1つのファイルへ永続化するストレージエンジン。
pub struct Storage {
    /// このストレージ本体(テーブル定義・データページ)のファイルパス。
    /// 索引ファイル([`index_file_path`])を組み立てるために保持する(第24章)。
    path: PathBuf,
    pool: BufferPool,
    next_table_id: u64,
    tables: HashMap<TableId, TableEntry>,
    /// `DROP TABLE`によって空いた、再利用待ちのページの一覧。
    free_pages: Vec<PageId>,
    fsm: FreeSpaceMap,
    /// `CREATE INDEX`で登録された索引(第24章)。キーは索引名。
    indexes: HashMap<String, IndexEntry>,
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
        let path_buf = path.as_ref().to_path_buf();
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
            path: path_buf,
            pool,
            next_table_id: 0,
            tables: HashMap::new(),
            free_pages: Vec::new(),
            fsm: FreeSpaceMap::new(),
            indexes: HashMap::new(),
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
        let path_buf = path.as_ref().to_path_buf();
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

        // 索引メタデータから、索引ごとの専用ファイル(モジュール冒頭を参照)を
        // 開き直す。索引の実データ(BTree本体)はCatalogページには無く、
        // それぞれのファイルの中にMetaページとして永続化されている
        // (`crate::btree::BTree::open`)。
        let mut indexes = HashMap::new();
        for info in decoded.indexes {
            let index_path = index_file_path(&path_buf, &info.name);
            let index_disk = DiskManager::open(&index_path)?;
            let btree = BTree::open(BufferPool::new(index_disk, DEFAULT_BUFFER_POOL_CAPACITY))?;
            indexes.insert(info.name.clone(), IndexEntry { info, btree });
        }

        Ok(Storage {
            path: path_buf,
            pool,
            next_table_id: decoded.next_table_id,
            tables: decoded.tables,
            free_pages: decoded.free_pages,
            fsm,
            indexes,
        })
    }

    /// キャッシュされているdirtyなページをすべてディスクへ書き戻す。
    ///
    /// `HeapFile::flush`(第14章)と同じく、`BufferPool::flush_all`をそのまま
    /// 呼ぶだけの薄いラッパーである。OSへの書き渡しまでで、実ディスクへの
    /// 同期([`Storage::sync`])までは行わない。
    ///
    /// 第24章から、索引は本体とは別ファイル(モジュール冒頭を参照)なので、
    /// 索引ごとに`BTree::flush`も呼ぶ。本体の`self.pool`だけをflushして
    /// 索引側を忘れると、索引に加えた変更(`CREATE INDEX`のIndex Build、
    /// Index Maintenance)がキャッシュに残ったまま、プロセスの再起動で
    /// 失われてしまう。
    pub fn flush(&self) -> DbResult<()> {
        self.pool.flush_all()?;
        for entry in self.indexes.values() {
            entry.btree.flush()?;
        }
        Ok(())
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
    /// 話題(グループコミットなど)は第33章のWALで扱う。[`Self::flush`]と同じ理由で、
    /// 索引ごとに`BTree::sync`も呼ぶ(第24章)。
    pub fn sync(&self) -> DbResult<()> {
        self.pool.sync()?;
        for entry in self.indexes.values() {
            entry.btree.sync()?;
        }
        Ok(())
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
    /// 一意性が壊れる、のどちらも避けるため)。`Storage::open`は
    /// `next_table_id == u64::MAX`のカタログを「有効な`TableId`を払い出し
    /// 尽くした」という正当な状態として受理し、拒否しない(`validate_table_metadata`
    /// のドキュメントを参照)。つまりこの分岐は`Storage::open`側の防御の
    /// すり抜けを拾う二重の備えではなく、`next_table_id`の上限をここ
    /// (`create_table`)だけで一元的に守るための唯一の関所である。
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
    ///
    /// 第24章から、このテーブルに対応する索引(`CREATE INDEX`で作られたもの、
    /// `PRIMARY KEY`・`UNIQUE`列に自動で作られたものの両方)も[`Self::drop_index`]
    /// と同じ手順でまとめて削除する。索引だけをテーブルの削除後に取り残すと、
    /// もう存在しないテーブルを指す索引メタデータがカタログに残ってしまう。
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

        let index_names: Vec<String> =
            self.indexes.values().filter(|e| e.info.table_id == id).map(|e| e.info.name.clone()).collect();
        for index_name in index_names {
            self.drop_index(&index_name)?;
        }

        self.persist_catalog()?;
        Ok(id)
    }

    /// 索引名から[`IndexInfo`]を引く。見つからなければ`None`を返す。
    pub fn index(&self, name: &str) -> Option<&IndexInfo> {
        self.indexes.get(name).map(|e| &e.info)
    }

    /// `table_id`のテーブルに対応する全索引の[`IndexInfo`]を返す(第24章)。
    /// `crate::executor::storage_insert`等のIndex Maintenanceが、挿入・更新・
    /// 削除された行についてどの索引を更新すべきかを求めるために使う。
    pub fn indexes_for_table(&self, table_id: TableId) -> impl Iterator<Item = &IndexInfo> {
        self.indexes.values().filter(move |e| e.info.table_id == table_id).map(|e| &e.info)
    }

    /// `table_id`のテーブルの`column_index`番目の列に対応する`UNIQUE`索引の
    /// `BTree`を引く(第24章)。`crate::index::check_uniqueness_with_index`が、
    /// 第20章の走査ベース一意性検査の代わりにこの索引へ`lookup`するために使う。
    pub(crate) fn unique_index_for_column(&self, table_id: TableId, column_index: usize) -> Option<&BTree> {
        self.indexes
            .values()
            .find(|e| e.info.table_id == table_id && e.info.column_index == column_index && e.info.unique)
            .map(|e| &e.btree)
    }

    /// `index_name`という名前で、`table_name`の`column_name`列を索引化した
    /// B+Tree索引を新しく作る(`CREATE INDEX`、第24章)。`unique`は
    /// `CREATE UNIQUE INDEX`かどうかで、この経路(SQL構文)から作る索引は常に
    /// [`IndexInfo::primary_key`]が`false`になる。`PRIMARY KEY`列に対応する
    /// 索引は[`Self::create_constraint_index`]を使う。
    pub fn create_index(&mut self, index_name: &str, table_name: &str, column_name: &str, unique: bool) -> DbResult<()> {
        self.create_index_impl(index_name, table_name, column_name, unique, false)
    }

    /// `PRIMARY KEY`・`UNIQUE`列に自動でユニーク索引を作る(第24章、
    /// `Database::execute_create_table`が呼ぶ)。`primary_key`が`true`なら、
    /// この索引で検出した重複は`DbError::PrimaryKeyViolation`として報告される
    /// ([`IndexInfo::primary_key`]を参照)。この索引は常に`unique = true`である。
    pub(crate) fn create_constraint_index(
        &mut self,
        index_name: &str,
        table_name: &str,
        column_name: &str,
        primary_key: bool,
    ) -> DbResult<()> {
        self.create_index_impl(index_name, table_name, column_name, true, primary_key)
    }

    /// [`Self::create_index`]・[`Self::create_constraint_index`]の共通実装。
    ///
    /// 索引名がすでに使われている場合は`DbError::DuplicateIndex`、テーブルまたは
    /// 列が存在しない場合は`DbError::TableNotFound`・`DbError::CorruptCatalog`
    /// (どちらも通常は`Binder`がすでに検査済みで到達しない)を返す。
    ///
    /// **Index Build**: 索引ごとに専用のファイル(`index_file_path`)を新しく
    /// 作り、`table_name`の既存の全行を`scan`しながら、対象列が`NULL`でない
    /// 行だけを`BTree::insert`する。`unique`が`true`で、既存行の中にすでに
    /// 重複するキーがあった場合は、`crate::btree::DbError::BTreeUniqueViolation`を
    /// `primary_key`に応じて第20章と同じ`DbError::PrimaryKeyViolation`・
    /// `DbError::UniqueViolation`(列名つき)へ翻訳して返し、作りかけの
    /// 索引ファイルを削除する。
    fn create_index_impl(
        &mut self,
        index_name: &str,
        table_name: &str,
        column_name: &str,
        unique: bool,
        primary_key: bool,
    ) -> DbResult<()> {
        if self.indexes.contains_key(index_name) {
            return Err(DbError::DuplicateIndex(index_name.to_string()));
        }
        let table_info = self.table(table_name).ok_or_else(|| DbError::TableNotFound(table_name.to_string()))?.clone();
        let column_index = table_info
            .schema
            .index_of(column_name)
            .ok_or_else(|| DbError::CorruptCatalog(format!("列が見つかりません: {column_name}")))?;
        let key_type = table_info.schema.columns()[column_index].data_type;

        let index_path = index_file_path(&self.path, index_name);
        let disk = DiskManager::open(&index_path)?;
        let mut btree = BTree::create(BufferPool::new(disk, DEFAULT_BUFFER_POOL_CAPACITY), key_type, unique)?;

        // Index Build: 既存の全行を読み、対象列がNULLでない行だけを挿入する。
        let mut pairs: Vec<(crate::types::Value, RecordId)> = Vec::new();
        for entry in self.scan(table_info.id)? {
            let (rid, bytes) = entry?;
            let tuple = decode_tuple(&table_info.schema, &bytes)?;
            let value = tuple.get(column_index).expect("tupleはschemaと同じ列数を持つ").clone();
            if !value.is_null() {
                pairs.push((value, rid));
            }
        }
        for (value, rid) in &pairs {
            if let Err(err) = btree.insert(value, *rid) {
                drop(btree);
                let _ = std::fs::remove_file(&index_path);
                return Err(translate_btree_error(err, primary_key, column_name, value));
            }
        }

        let info = IndexInfo {
            name: index_name.to_string(),
            table_id: table_info.id,
            column_index,
            column_name: column_name.to_string(),
            unique,
            primary_key,
            key_type,
        };
        self.indexes.insert(index_name.to_string(), IndexEntry { info, btree });

        if let Err(err) = self.persist_catalog() {
            self.indexes.remove(index_name);
            let _ = std::fs::remove_file(&index_path);
            return Err(err);
        }
        Ok(())
    }

    /// `index_name`の索引を削除する(`DROP INDEX`、第24章)。
    ///
    /// 索引が存在しない場合は`DbError::IndexNotFound`を返す。カタログからの
    /// 削除に成功したら、その索引専用のファイル([`index_file_path`])を
    /// 削除する。ファイルの削除は`drop_table`(第15章)がテーブルのページを
    /// 即座にはファイルから取り除かない(Free Page Listへ積むだけ)のとは違い、
    /// 索引は他のどのテーブル・索引ともページを共有しない専用ファイルなので、
    /// そのまま`std::fs::remove_file`できる。
    pub fn drop_index(&mut self, index_name: &str) -> DbResult<()> {
        if self.indexes.remove(index_name).is_none() {
            return Err(DbError::IndexNotFound(index_name.to_string()));
        }
        self.persist_catalog()?;
        let index_path = index_file_path(&self.path, index_name);
        std::fs::remove_file(&index_path)?;
        Ok(())
    }

    /// 新しく挿入(または`UPDATE`で書き直され)た行`tuple`(`RecordId`は`rid`)に
    /// ついて、`table_id`の全索引(`UNIQUE`・非`UNIQUE`の両方)を更新する
    /// (Index Maintenance、第24章)。
    ///
    /// 索引化された列の値が`NULL`の行はどの索引にも登録しない
    /// (`crate::btree::BTree`のモジュールドキュメント「`NULL`はキーにしない」を
    /// 参照)。`UNIQUE`索引で重複が見つかった場合は`DbError::PrimaryKeyViolation`・
    /// `DbError::UniqueViolation`を返す。呼び出し側(`crate::executor`)は、
    /// この関数を呼ぶ前に`crate::index::check_uniqueness_with_index`で
    /// 検査を終えている前提のため、通常はここで初めて違反が見つかることはない。
    pub fn index_insert_row(&mut self, table_id: TableId, tuple: &Tuple, rid: RecordId) -> DbResult<()> {
        for entry in self.indexes.values_mut().filter(|e| e.info.table_id == table_id) {
            let Some(value) = tuple.get(entry.info.column_index) else { continue };
            if value.is_null() {
                continue;
            }
            entry
                .btree
                .insert(value, rid)
                .map_err(|err| translate_btree_error(err, entry.info.primary_key, &entry.info.column_name, value))?;
        }
        Ok(())
    }

    /// 削除(または`UPDATE`で書き直される前)の行`tuple`(`RecordId`は`rid`)に
    /// ついて、`table_id`の全索引からエントリを取り除く(Index Maintenance、
    /// 第24章)。`crate::btree::BTree::delete`と同じくLazy Deleteであり、
    /// 索引側のページの占有率が下がってもMergeはしない。
    pub fn index_delete_row(&mut self, table_id: TableId, tuple: &Tuple, rid: RecordId) -> DbResult<()> {
        for entry in self.indexes.values_mut().filter(|e| e.info.table_id == table_id) {
            let Some(value) = tuple.get(entry.info.column_index) else { continue };
            if value.is_null() {
                continue;
            }
            entry.btree.delete(value, rid)?;
        }
        Ok(())
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

    /// 現在のテーブル定義・Free Page List・索引メタデータ(第24章)を
    /// `encode_catalog`でバイト列へ変換し、Catalogページへ書き込む。
    ///
    /// エンコード結果がCatalogページ1枚(`PAGE_PAYLOAD_SIZE`バイト)を超える場合は
    /// `DbError::CatalogTooLarge`を返す(モジュール冒頭の説明を参照)。
    fn persist_catalog(&self) -> DbResult<()> {
        let index_infos: Vec<&IndexInfo> = self.indexes.values().map(|e| &e.info).collect();
        let bytes = encode_catalog(self.next_table_id, &self.tables, &self.free_pages, &index_infos);
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
/// - 各テーブルの`TableId`が`next_table_id`未満であること(そうでなければ、
///   次に`create_table`したテーブルが同じ`TableId`を再利用してしまう)。
/// - テーブル名が重複していないこと(`TableId`自体の重複は`decode_catalog`が
///   デコードの時点で検出済み)。
///
/// `next_table_id == u64::MAX`はここでは拒まない。
/// これは「有効な`TableId`をすべて払い出し尽くした」という正当な状態であり
/// (最後に払い出した`TableId`は`u64::MAX - 1`)、そのカタログを持つファイルは
/// 何度でも`open`できてよい。制限を課すべきなのは「新しい`TableId`を実際に
/// 払い出そうとする瞬間」であって、「そのファイルを開けるかどうか」ではない。
/// もし`open`の時点で`next_table_id == u64::MAX`を`CorruptCatalog`として
/// 拒んでいたら、`next_table_id == u64::MAX - 1`のカタログから
/// `create_table`をちょうど1回成功させて`next_table_id`が`u64::MAX`になった
/// 直後、そのファイルは二度と`open`できなくなってしまう(成功しただけの
/// 操作が、後から見ると「壊れたファイルを作った」ことになる)。この矛盾を
/// 避けるため、`next_table_id == u64::MAX`は`open`側では正当なsentinelとして
/// 受理し、実際にそこから先へ進もうとする`create_table`側だけを
/// `checked_add`(このモジュールの`Storage::create_table`を参照)で防ぐ。
fn validate_table_metadata(decoded: &DecodedCatalog) -> DbResult<()> {
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

/// `crate::btree::DbError::BTreeUniqueViolation`(列名を持たない、B+Tree自身の
/// エラー)を、`primary_key`に応じて第20章の`DbError::PrimaryKeyViolation`・
/// `DbError::UniqueViolation`(列名・値つき)へ翻訳する(第24章)。
/// `BTreeUniqueViolation`以外のエラーはそのまま素通しする(`NullKeyNotAllowed`
/// はここまでに`value.is_null()`で弾いてあるため、通常は起こらない)。
fn translate_btree_error(err: DbError, primary_key: bool, column_name: &str, value: &crate::types::Value) -> DbError {
    match err {
        DbError::BTreeUniqueViolation if primary_key => {
            DbError::PrimaryKeyViolation { column: column_name.to_string(), value: value.to_string() }
        }
        DbError::BTreeUniqueViolation => DbError::UniqueViolation { column: column_name.to_string(), value: value.to_string() },
        other => other,
    }
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
    /// 索引メタデータ(第24章)。索引名の重複が無いことは`decode_catalog`が
    /// `Vec`へ積む時点で検査済み。
    indexes: Vec<IndexInfo>,
}

/// 現在のテーブル定義・Free Page List・索引メタデータ(第24章)をバイト列へ
/// エンコードする。
///
/// テーブルは`TableId`の昇順、索引は索引名の昇順で書き出す。`tables`は
/// `HashMap`であり反復順が実行のたびに変わりうるため、書き出す順序を
/// 固定しておかないと、論理的には同じ状態でもエンコード結果のバイト列が
/// 実行のたびに変わってしまう(`indexes`は`Storage`側で`HashMap`から
/// `Vec<&IndexInfo>`へ変換済みで渡ってくるため、ここで並び順を確定させる)。
fn encode_catalog(
    next_table_id: u64,
    tables: &HashMap<TableId, TableEntry>,
    free_pages: &[PageId],
    indexes: &[&IndexInfo],
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
            out.push(u8::from(column.primary_key));
            out.push(u8::from(column.unique));
        }

        out.extend_from_slice(&(entry.page_ids.len() as u32).to_le_bytes());
        for &page_id in &entry.page_ids {
            out.extend_from_slice(&page_id.0.to_le_bytes());
        }
    }

    let mut sorted_indexes: Vec<&&IndexInfo> = indexes.iter().collect();
    sorted_indexes.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend_from_slice(&(sorted_indexes.len() as u32).to_le_bytes());
    for info in sorted_indexes {
        let name_bytes = info.name.as_bytes();
        out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(&info.table_id.0.to_le_bytes());
        out.extend_from_slice(&(info.column_index as u16).to_le_bytes());
        let column_name_bytes = info.column_name.as_bytes();
        out.extend_from_slice(&(column_name_bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(column_name_bytes);
        out.push(u8::from(info.unique));
        out.push(u8::from(info.primary_key));
        out.push(data_type_to_u8(info.key_type));
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
            let nullable = take_bool(&mut cursor, "nullable")?;
            let primary_key = take_bool(&mut cursor, "primary_key")?;
            let unique = take_bool(&mut cursor, "unique")?;
            let mut column = Column::new(col_name, data_type, nullable);
            if primary_key {
                column = column.with_primary_key();
            }
            if unique {
                column = column.with_unique();
            }
            columns.push(column);
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

    let index_count = take_u32(&mut cursor, "index_count")? as usize;
    let mut indexes = Vec::new();
    let mut seen_index_names = std::collections::HashSet::new();
    for _ in 0..index_count {
        let name_len = take_u16(&mut cursor, "索引名の長さ")? as usize;
        let name = take_string(&mut cursor, name_len, "索引名")?;
        let table_id = TableId(take_u64(&mut cursor, "索引のtable_id")?);
        let column_index = take_u16(&mut cursor, "索引のcolumn_index")? as usize;
        let column_name_len = take_u16(&mut cursor, "索引の列名の長さ")? as usize;
        let column_name = take_string(&mut cursor, column_name_len, "索引の列名")?;
        let unique = take_bool(&mut cursor, "索引のunique")?;
        let primary_key = take_bool(&mut cursor, "索引のprimary_key")?;
        let key_type = data_type_from_u8(take_u8(&mut cursor, "索引のkey_type")?)?;

        if !seen_index_names.insert(name.clone()) {
            // encode_catalogが索引名の一意性を保証しているHashMap<String, _>を
            // 経由していれば起こらないが、`decode_catalog`は入力を信用しない
            // (テーブル名の重複検出=`TableId`の重複検出と同じ理由)。
            return Err(DbError::CorruptCatalog(format!("索引名'{name}'が複数回出現しています")));
        }
        indexes.push(IndexInfo { name, table_id, column_index, column_name, unique, primary_key, key_type });
    }

    Ok(DecodedCatalog {
        next_table_id,
        tables,
        free_pages,
        indexes,
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

/// `0`または`1`の1バイトを`bool`として読む。それ以外の値は
/// `DbError::CorruptCatalog`にする。`nullable`・`primary_key`・`unique`
/// (第20章で追加)が共通して使う。
fn take_bool(bytes: &mut &[u8], what: &str) -> DbResult<bool> {
    match take_u8(bytes, what)? {
        0 => Ok(false),
        1 => Ok(true),
        other => Err(DbError::CorruptCatalog(format!(
            "{what}は0か1である必要がありますが{other}でした"
        ))),
    }
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
    use crate::types::Value;
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
        let bytes = encode_catalog(1, &tables, &[PageId(0)], &[]);
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
        let bytes = encode_catalog(1, &tables, &[], &[]);
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
            let bytes = encode_catalog(1, &tables, &[], &[]);
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
        let bytes = encode_catalog(2, &tables, &[], &[]);
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
        let bytes = encode_catalog(0, &tables, &[], &[]);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_accepts_a_catalog_whose_next_table_id_is_u64_max_as_a_valid_sentinel() {
        // next_table_id=u64::MAX、table_count=0という、checksum・構造ともに
        // 正常なカタログ。これは「有効なTableIdを払い出し尽くした」という
        // 正当な状態であり、openはこれを拒んではならない
        // (validate_table_metadataのドキュメントを参照)。次にcreate_tableを
        // 呼んだときだけ、checked_addがTableIdSpaceExhaustedとして拒む。
        let path = temp_path("next-table-id-u64-max");
        Storage::create(&path).unwrap();

        let tables: HashMap<TableId, TableEntry> = HashMap::new();
        let bytes = encode_catalog(u64::MAX, &tables, &[], &[]);
        write_catalog_payload(&path, &bytes);

        let mut storage = Storage::open(&path).unwrap();
        assert_eq!(storage.next_table_id, u64::MAX);
        let err = expect_err(storage.create_table("a", users_schema()));
        assert!(matches!(err, DbError::TableIdSpaceExhausted));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn create_table_rejects_when_next_table_id_would_overflow() {
        // next_table_idがu64::MAXに達している状態は、Storage::openが正当な
        // sentinelとして受理する(validate_table_metadataのドキュメントを
        // 参照)。オーバーフローを防ぐ関所はcreate_table自身のchecked_add
        // だけであり、ここではそれが実際にTableIdSpaceExhaustedを返すことを
        // 確認する。
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
    fn a_table_created_right_before_the_table_id_space_is_exhausted_survives_a_reopen() {
        // next_table_id=u64::MAX-1のカタログから、まさに最後の1つとなる
        // create_tableを成功させる。永続化されたnext_table_idはu64::MAXに
        // なるが、それでもreopenは成功しなければならない(u64::MAXは
        // Storage::openが拒む対象ではなく正当なsentinelである)。reopen後、
        // 次のcreate_tableだけがTableIdSpaceExhaustedで失敗する。
        let path = temp_path("last-table-id-before-exhaustion");
        {
            let mut storage = Storage::create(&path).unwrap();
            storage.next_table_id = u64::MAX - 1;
            let id = storage.create_table("last", users_schema()).unwrap();
            assert_eq!(id, TableId(u64::MAX - 1));
            assert_eq!(storage.next_table_id, u64::MAX);
            storage.flush().unwrap();
        }

        let mut storage = Storage::open(&path).unwrap();
        assert_eq!(storage.next_table_id, u64::MAX);
        assert_eq!(storage.table("last").unwrap().id, TableId(u64::MAX - 1));

        let err = expect_err(storage.create_table("one_more", users_schema()));
        assert!(matches!(err, DbError::TableIdSpaceExhausted));

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
        let bytes = encode_catalog(2, &tables, &[], &[]);
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

    // ---- 第24章: CREATE INDEX / DROP INDEX / Index Maintenance ----

    fn index_test_paths(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let db_path = temp_path(name);
        let idx_path = index_file_path(&db_path, "idx");
        (db_path, idx_path)
    }

    /// `users_schema()`(`id: BIGINT`, `name: TEXT`)の1行を、`Storage::insert`が
    /// 期待する`crate::tuple_codec::encode_tuple`済みのバイト列として書き込む。
    /// (テストの中で`storage.insert(table_id, b"alice")`のように生の文字列
    /// バイト列を渡すのは`get`だけを確認するテストでは問題ないが、
    /// `Storage::create_index`のIndex Buildは`decode_tuple`で読み戻すため、
    /// 正しくエンコードされたタプルが必要になる。)
    fn insert_user_row(storage: &mut Storage, table_id: TableId, id: i64, name: &str) -> RecordId {
        let schema = users_schema();
        let tuple = Tuple::new(&schema, vec![Value::BigInt(id), Value::Text(name.to_string())]).unwrap();
        let bytes = crate::tuple_codec::encode_tuple(&schema, &tuple);
        storage.insert(table_id, &bytes).unwrap()
    }

    #[test]
    fn create_index_builds_from_existing_rows() {
        let (path, idx_path) = index_test_paths("create-index-build");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();
        insert_user_row(&mut storage, table_id, 1, "alice");
        insert_user_row(&mut storage, table_id, 2, "bob");

        storage.create_index("idx", "users", "name", false).unwrap();
        let info = storage.index("idx").unwrap();
        assert_eq!(info.table_id, table_id);
        assert_eq!(info.column_name, "name");
        assert!(!info.unique);

        let index = storage.unique_index_for_column(table_id, 1);
        assert!(index.is_none(), "unique=falseで作った索引はunique_index_for_columnに現れない");

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&idx_path);
    }

    #[test]
    fn create_index_rejects_a_duplicate_name() {
        let (path, idx_path) = index_test_paths("create-index-dup-name");
        let mut storage = Storage::create(&path).unwrap();
        storage.create_table("users", users_schema()).unwrap();
        storage.create_index("idx", "users", "name", false).unwrap();
        let err = expect_err(storage.create_index("idx", "users", "id", false));
        assert!(matches!(err, DbError::DuplicateIndex(name) if name == "idx"));

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&idx_path);
    }

    #[test]
    fn create_unique_index_rejects_existing_duplicate_values() {
        let (path, idx_path) = index_test_paths("create-unique-index-existing-dup");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();
        insert_user_row(&mut storage, table_id, 1, "alice");
        insert_user_row(&mut storage, table_id, 2, "alice");

        let err = expect_err(storage.create_index("idx", "users", "name", true));
        assert!(matches!(err, DbError::UniqueViolation { .. }));
        // 失敗した索引はカタログにも残らず、専用ファイルも残らない。
        assert!(storage.index("idx").is_none());
        assert!(!idx_path.exists());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn drop_index_removes_metadata_and_file() {
        let (path, idx_path) = index_test_paths("drop-index");
        let mut storage = Storage::create(&path).unwrap();
        storage.create_table("users", users_schema()).unwrap();
        storage.create_index("idx", "users", "name", false).unwrap();
        assert!(idx_path.exists());

        storage.drop_index("idx").unwrap();
        assert!(storage.index("idx").is_none());
        assert!(!idx_path.exists());

        let err = expect_err(storage.drop_index("idx"));
        assert!(matches!(err, DbError::IndexNotFound(name) if name == "idx"));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn drop_table_also_drops_its_indexes() {
        let (path, idx_path) = index_test_paths("drop-table-drops-index");
        let mut storage = Storage::create(&path).unwrap();
        storage.create_table("users", users_schema()).unwrap();
        storage.create_index("idx", "users", "name", false).unwrap();

        storage.drop_table("users").unwrap();
        assert!(storage.index("idx").is_none());
        assert!(!idx_path.exists());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn index_insert_and_delete_row_maintain_a_unique_index() {
        let (path, idx_path) = index_test_paths("index-maintenance");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();
        storage.create_index("idx", "users", "name", true).unwrap();

        let schema = users_schema();
        let tuple = Tuple::new(&schema, vec![Value::BigInt(1), Value::Text("alice".to_string())]).unwrap();
        let rid = insert_user_row(&mut storage, table_id, 1, "alice");
        storage.index_insert_row(table_id, &tuple, rid).unwrap();

        // 同じ値をもう一度挿入しようとするとunique違反になる。
        let dup_rid = insert_user_row(&mut storage, table_id, 2, "alice-2");
        let err = expect_err(storage.index_insert_row(table_id, &tuple, dup_rid));
        assert!(matches!(err, DbError::UniqueViolation { column, .. } if column == "name"));

        // 削除すれば、同じ値を再び挿入できるようになる。
        storage.index_delete_row(table_id, &tuple, rid).unwrap();
        storage.index_insert_row(table_id, &tuple, dup_rid).unwrap();

        assert_eq!(
            storage.unique_index_for_column(table_id, 1).unwrap().lookup(&Value::Text("alice".to_string())).unwrap(),
            vec![dup_rid]
        );

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&idx_path);
    }

    #[test]
    fn index_maintenance_skips_null_values() {
        let (path, idx_path) = index_test_paths("index-maintenance-null");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();
        storage.create_index("idx", "users", "name", true).unwrap();

        let schema = users_schema();
        let null_tuple = Tuple::new(&schema, vec![Value::BigInt(1), Value::Null]).unwrap();
        let bytes = crate::tuple_codec::encode_tuple(&schema, &null_tuple);
        let rid1 = storage.insert(table_id, &bytes).unwrap();
        let rid2 = storage.insert(table_id, &bytes).unwrap();
        // NULLはunique索引にとって重複とみなされない(第20章と同じ規則)。
        storage.index_insert_row(table_id, &null_tuple, rid1).unwrap();
        storage.index_insert_row(table_id, &null_tuple, rid2).unwrap();

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&idx_path);
    }

    #[test]
    fn indexes_survive_a_reopen_and_keep_enforcing_uniqueness() {
        let (path, idx_path) = index_test_paths("index-reopen");
        {
            let mut storage = Storage::create(&path).unwrap();
            let table_id = storage.create_table("users", users_schema()).unwrap();
            assert_eq!(table_id, TableId(0));
            storage.create_index("idx", "users", "name", true).unwrap();
            let schema = users_schema();
            let tuple = Tuple::new(&schema, vec![Value::BigInt(1), Value::Text("alice".to_string())]).unwrap();
            let rid = insert_user_row(&mut storage, table_id, 1, "alice");
            storage.index_insert_row(table_id, &tuple, rid).unwrap();
            storage.flush().unwrap();
            storage.sync().unwrap();
        }

        let mut storage = Storage::open(&path).unwrap();
        let info = storage.index("idx").unwrap();
        assert!(info.unique);
        assert_eq!(info.column_name, "name");

        let schema = users_schema();
        let dup = Tuple::new(&schema, vec![Value::BigInt(2), Value::Text("alice".to_string())]).unwrap();
        let dup_rid = insert_user_row(&mut storage, TableId(0), 2, "alice-2");
        let err = expect_err(storage.index_insert_row(TableId(0), &dup, dup_rid));
        assert!(matches!(err, DbError::UniqueViolation { .. }));

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&idx_path);
    }
}
