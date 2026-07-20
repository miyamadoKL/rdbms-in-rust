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
//! `nullable`の直後で次の列(または`page_count`)を読もうとして境界がずれる。
//!
//! この時点では、境界がずれた読み出しが`DbError::CorruptCatalog`になることを
//! 「レイアウトが変わったファイルは開けない」という設計上の帰結として
//! 説明していた。ずれた読み出しの先で、いずれ文字列の長さプレフィックス
//! (`name_len`・`col_name_len`)を大きく誤読し、残りバイト数を超える範囲を
//! 要求して`take`の境界検査に引っかかる、という筋道である。だが
//! Catalogページの`payload`は`PAGE_PAYLOAD_SIZE`バイト固定で、実データの
//! 直後から末尾まで`0`で埋められている(`persist_catalog`を参照)ため、
//! ずれた読み出しの結果が**たまたま小さい値**(索引が1本だけの場合の
//! `is_constraint`の後続バイトなど)になると、境界検査に一度も引っかからず
//! 静かに「妥当に見える」値を組み立ててしまう経路が実際に存在する
//! (第3部レビューで、索引メタデータへの`is_constraint`追加時に実例が
//! 見つかった)。つまり、この「いずれ`CorruptCatalog`になるはず」という
//! 説明は、レイアウト変更の内容によっては成り立たない場合がある。
//! この教材が確実な保証として採用している方式は、後述の
//! [`CATALOG_MAGIC`]・[`CATALOG_LAYOUT_VERSION`]による判定である。
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
//!     is_constraint:   u8 (0 または 1、第3部2巡目レビュー対応)
//! ```
//!
//! この章より前(第23章以前)に`Storage::create`で作られたファイルはこの
//! セクションを持たない。この教材は章をまたいだファイル互換性を約束しない
//! 方針だが(モジュール冒頭の「第20章での変更」節を参照)、「約束しない」ことと
//! 「古い形式を開こうとすると確実にエラーになる」ことは別の話である。
//! 索引が1本もないファイルでは、この章のコードは`index_count`を読む位置に
//! 残っている`0`埋めの`payload`をそのまま`index_count = 0`として受理して
//! しまい、`DbError::CorruptCatalog`にすらならない(索引が無いという結論
//! 自体はたまたま正しいので、実害は無いまま素通りする)。
//!
//! # 第3部レビュー対応: レイアウトの版を先頭に埋め込み、確実に拒否する
//!
//! `is_constraint`(索引が自動生成された制約索引かどうか、`crate::index::IndexInfo`を
//! 参照)を追加したとき、この「約束しないが、たいていはエラーになるはず」
//! という説明の弱さが実際の不具合として表面化した。索引を1本だけ持つ
//! 旧いカタログを新しいコードで開くと、`is_constraint`を読む位置に残っている
//! `0`埋めの`payload`が`is_constraint = false`としてそのまま受理されてしまい、
//! `DROP INDEX`が制約索引まで削除できてしまう(`crate::storage`のテスト
//! `open_rejects_a_pre_is_constraint_catalog_with_a_single_constraint_index`が
//! この状態を再現する)。索引が複数本ある場合は、1本目のレコードが1バイト
//! 短く読まれることで2本目以降のフィールド境界そのものがずれ、無関係な
//! 整合性エラーとして観測される。
//!
//! この種の不具合を個別のフィールドごとに塞ぐのではなく、レイアウトが
//! 変わったこと自体を`payload`の**先頭**で検出できるようにする。
//!
//! ```text
//! magic:          u8 × 8  (CATALOG_MAGIC、固定のASCII文字列)
//! layout_version: u32     (CATALOG_LAYOUT_VERSION)
//! (以降、next_table_idから続くこのモジュールのレイアウト)
//! ```
//!
//! `encode_catalog`は`payload`の先頭に、固定のマジックバイト列
//! ([`CATALOG_MAGIC`])と、現在のレイアウト版([`CATALOG_LAYOUT_VERSION`])を
//! 書く。`decode_catalog`はこの2つを最初に検査し、マジックバイト列が
//! 一致しないか、レイアウト版が現在のコードが理解する値と異なる場合は、
//! それ以降のバイト列を一切解釈せずに`DbError::CorruptCatalog`を返す。
//!
//! マジックバイト列を単なる版番号ではなく固定のASCII文字列にしてあるのは、
//! 版番号だけでは「古いファイルの`next_table_id`(この位置に来る値)が
//! たまたま現在の版番号と同じ小さな整数である」という偶然の一致を排除
//! できないからである。8バイトの固定文字列と偶然一致する確率は無視できる。
//!
//! **索引メタデータ・テーブル定義・Free Page Listのいずれかのレイアウトを
//! 今後変更する場合は、`CATALOG_LAYOUT_VERSION`を必ず1つ増やす。**
//! 増やし忘れると、この節が解決したのと同じ「たまたま妥当に見える値を
//! 静かに受理してしまう」不具合が再発しうる。
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
//!
//! # 第27章での変更: 統計情報をCatalogページへ追加する
//!
//! `ANALYZE`(第27章)が集めるテーブル・列ごとの統計情報
//! (`crate::statistics::TableStats`)を、索引メタデータと同じ考え方で
//! Catalogページの末尾に追記する。`ANALYZE`を実行していないテーブルは
//! このセクションに現れない(統計を持たないテーブルは、`estimator`モジュールが
//! デフォルトの選択率にフォールバックして扱う)。
//!
//! ```text
//! stats_count: u32
//! stats × stats_count:
//!     table_id:      u64
//!     row_count:     u64
//!     column_count:  u16
//!     columns × column_count:
//!         null_count:     u64
//!         distinct_count: u64
//!         min:            Value  (`NULL`タグは「値が無い」ことを表す)
//!         max:            Value
//!         bucket_count:   u16
//!         buckets × bucket_count:
//!             lower:     Value
//!             upper:     Value
//!             row_count: u64
//! ```
//!
//! `Value`は、既存のフィールドが使ってきた`data_type: u8`とは別に、値そのものを
//! 復元できるよう`tag: u8`(0=NULL, 1=BOOLEAN, 2=BIGINT, 3=TEXT)に続けて
//! 型ごとのペイロードを書く小さな自己記述形式でエンコードする
//! (`encode_value`/`decode_value`)。`min`・`max`は`Option<Value>`だが、
//! 列に非NULLの値が1件も無い場合(=`None`)しか`NULL`タグを取らない
//! (`StatsCollector`はそもそも`NULL`値を`min`/`max`の対象に含めない)ため、
//! 「値が無い」ことを表す専用のフラグバイトを別に持たせず、`Value::Null`の
//! タグをそのまま「無し」の意味で流用する。
//!
//! 索引メタデータと同じ理由で、テーブルは`TableId`の昇順に書き出す。この
//! セクションを追加したことに伴い、[`CATALOG_LAYOUT_VERSION`]を`1`から`2`へ
//! 上げてある。
//!
//! # 第4部レビュー対応: MCV(最頻値)の追加と、統計の意味検証
//!
//! `crate::statistics::ColumnStats`にMCV(最頻値、`mcv`)を追加した
//! (`crate::statistics`モジュールの説明を参照)ことに伴い、列ごとの
//! レコードへ`mcv`のセクションを追加する。既存の`bucket_count`・`buckets`の
//! 直前に挿入する(`min`・`max`の直後)。
//!
//! ```text
//! columns × column_count:
//!     null_count:     u64
//!     distinct_count: u64
//!     min:            Value
//!     max:            Value
//!     mcv_count:      u16
//!     mcv × mcv_count:
//!         value: Value
//!         count: u64
//!     bucket_count:   u16
//!     buckets × bucket_count:
//!         lower:     Value
//!         upper:     Value
//!         row_count: u64
//! ```
//!
//! フィールドを列の途中に挿入する以上、既存のフィールドをすべて後ろへ
//! ずらすことになるため、[`CATALOG_LAYOUT_VERSION`]を`2`から`3`へ上げてある。
//!
//! この変更と合わせて、`decode_catalog`が構文的に復元した統計情報が、
//! 対応するテーブル定義と意味的に整合しているか(列数・型が一致するか、
//! `null_count`が`row_count`を超えていないか、バケツの境界が昇順に並んで
//! いるか等)を検査する`validate_stats_metadata`を追加した。`decode_catalog`
//! 自身が検出するのは、統計に紐づく`TableId`の重複だけであり、テーブル定義
//! との整合性までは検査していなかった(このセクションの追加前から存在した
//! 欠落)。`validate_stats_metadata`は`Storage::open`(カタログを復元する経路)
//! と、`Storage::set_table_stats`(`ANALYZE`が新しい統計を登録する経路)の
//! 両方から呼ぶ。検査項目の詳細は`validate_stats_metadata`のドキュメントを
//! 参照。

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::btree::BTree;
use crate::buffer_pool::{BufferPool, BufferPoolStats};
use crate::catalog::TableInfo;
use crate::disk_manager::DiskManager;
use crate::error::{DbError, DbResult};
use crate::free_space_map::FreeSpaceMap;
use crate::heap_file::Scan;
use crate::ids::{Lsn, PageId, RecordId, TableId, TransactionId};
use crate::index::IndexInfo;
use crate::page::{PAGE_PAYLOAD_SIZE, PageType};
use crate::slotted_page::{SlotStatus, SlottedPage, SlottedPageRef, max_len_for_fresh_page};
use crate::statistics::{Bucket, ColumnStats, HISTOGRAM_BUCKET_COUNT, MCV_MAX_ENTRIES, TableStats};
use crate::tuple_codec::decode_tuple;
use crate::types::{Column, DataType, Schema, Tuple, Value, compare_values};
use crate::wal::WalWriter;

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

/// `rebuild_one_index_after_recovery`が索引を作り直す間だけ使う、一時ファイルの
/// パス(第34章、`Storage::rebuild_all_indexes_after_recovery`を参照)。
/// 索引の本体ファイルと同じディレクトリに置くことで、`std::fs::rename`が
/// 同一ファイルシステム内のアトミックな置き換えになることを保証する。
fn index_rebuild_temp_path(db_path: &Path, index_name: &str) -> PathBuf {
    let mut os_string = index_file_path(db_path, index_name).into_os_string();
    os_string.push(".rebuilding");
    PathBuf::from(os_string)
}

/// `db_path`のWALファイル(第33章)のパスを組み立てる。索引ファイル
/// ([`index_file_path`])と同じ命名の流儀で、本体のデータファイルとは
/// 別のファイル`<db_path>.wal`に置く。
fn wal_file_path(db_path: &Path) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".wal");
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
    /// `ANALYZE`で収集された統計情報(第27章)。キーは`TableId`。`ANALYZE`を
    /// 一度も実行していないテーブルはここに現れない。
    stats: HashMap<TableId, TableStats>,
    /// 索引名ごとの`BTree::lookup`・`BTree::range`の呼び出し回数(第39章、
    /// `SHOW STATS`が表示する)。`IndexScanExec`・`IndexNestedLoopJoinExec`
    /// (`crate::physical_plan`)が索引を1回引くたびに1つ増える。永続化しない
    /// (プロセスの起動からの累積値であり、`FreeSpaceMap`と同じくディスク上の
    /// カタログには書かない)。`Storage`を`&self`のまま読む実行経路
    /// (`IndexScanExec`等は`&'a Storage`しか持たない)から増やす必要が
    /// あるため、`RefCell`で内部可変性を持たせる。
    index_usage: RefCell<HashMap<String, u64>>,
    /// このテーブル本体用のWAL(第33章)。`<path>.wal`という専用ファイルを持ち、
    /// `pool`(テーブル本体の`BufferPool`)に[`BufferPool::attach_wal`]で
    /// 結線してある。`Database`は`Storage::wal`経由でこの`Arc`を共有し、
    /// `BEGIN`・`COMMIT`・`ROLLBACK`のログレコードを直接書く
    /// (`crate::database`、`crate::transaction::apply_wal_undo_disk`)。
    /// 索引ごとの`BTree`は別々の`BufferPool`を持つが、そちらにはWALを結線
    /// しない(モジュール冒頭「この章が対象にする範囲」を参照)。
    wal: Arc<Mutex<WalWriter>>,
    /// `Storage::open`が実行したCrash Recovery(第34章)の要約。
    /// `Storage::create`(新規作成)では常に`None`(Recoveryを行わないため)。
    last_recovery: Option<crate::recovery::RecoveryReport>,
}

/// `Storage::vacuum_table`が1テーブルぶんの回収結果として返す要約
/// (第39章)。`Database::execute_vacuum`のコマンドタグと、テストが回収の
/// 効果を実測するために使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VacuumReport {
    /// Free Page Listへ返した(完全に空になった)データページの枚数。
    pub reclaimed_pages: usize,
    /// 作り直した索引の本数。
    pub rebuilt_indexes: usize,
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

        let wal = Arc::new(Mutex::new(WalWriter::open(wal_file_path(&path_buf))?));
        pool.attach_wal(wal.clone());

        let storage = Storage {
            path: path_buf,
            pool,
            next_table_id: 0,
            tables: HashMap::new(),
            free_pages: Vec::new(),
            fsm: FreeSpaceMap::new(),
            indexes: HashMap::new(),
            stats: HashMap::new(),
            index_usage: RefCell::new(HashMap::new()),
            wal,
            last_recovery: None,
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
        validate_stats_metadata(&decoded.tables, &decoded.stats)?;

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

        let wal = Arc::new(Mutex::new(WalWriter::open(wal_file_path(&path_buf))?));
        pool.attach_wal(wal.clone());

        let mut storage = Storage {
            path: path_buf,
            pool,
            next_table_id: decoded.next_table_id,
            tables: decoded.tables,
            free_pages: decoded.free_pages,
            fsm,
            indexes,
            stats: decoded.stats,
            index_usage: RefCell::new(HashMap::new()),
            wal,
            last_recovery: None,
        };
        // Crash Recovery(第34章): WALのAnalysis→Redo→Undoを行い、前回の
        // クラッシュが残した「Commit済みだがまだページに届いていない変更」を
        // 再現し、「Commitされていない変更」を取り消す。新規作成
        // (`Storage::create`)にはこの手順は無く、既存ファイルを開く
        // このパスだけが通る(`crate::recovery`モジュールドキュメントを参照)。
        let report = crate::recovery::recover(&mut storage)?;
        storage.last_recovery = Some(report);
        Ok(storage)
    }

    /// 直近の`Storage::open`が実行したCrash Recovery(第34章)の要約。
    /// `Storage::create`で作ったばかりの(Recoveryを行っていない)場合は`None`。
    pub fn last_recovery_report(&self) -> Option<crate::recovery::RecoveryReport> {
        self.last_recovery
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

    /// このテーブル本体用のWAL(第33章)への共有ハンドル。
    ///
    /// `crate::database::Database`が`BEGIN`・`COMMIT`・`ROLLBACK`の
    /// Begin・Commit・Abortレコードを直接書き、`crate::executor`の
    /// `storage_insert`・`storage_update`・`storage_delete`がInsert・Update・
    /// Deleteレコードを書く(`crate::wal::WalCursor`経由)ときに使う。
    pub(crate) fn wal(&self) -> &Arc<Mutex<WalWriter>> {
        &self.wal
    }

    /// `page_id`のPage LSN(第33章)を`lsn`まで引き上げる。
    ///
    /// `insert`・`update`・`delete`で書き込んだページの`PageId`に対して、
    /// 対応するWALレコードを`append`した直後に呼ぶ薄い委譲であり、
    /// `crate::buffer_pool::BufferPool::bump_page_lsn`を呼ぶだけである。
    pub(crate) fn stamp_page_lsn(&self, page_id: PageId, lsn: Lsn) {
        self.pool.bump_page_lsn(page_id, lsn);
    }

    /// `page_id`の現在のPage LSN(第34章)。`crate::recovery`のRedoが
    /// 冪等性を判定するために使う(`crate::buffer_pool::BufferPool::page_lsn`
    /// への薄い委譲)。
    pub(crate) fn page_lsn(&self, page_id: PageId) -> DbResult<Lsn> {
        self.pool.page_lsn(page_id)
    }

    /// これまでにWALへ書いた全レコードを、開発者が目視で確認できる文字列へ
    /// 整形して返す(第33章、`crate::wal::WalWriter::dump`)。
    pub fn wal_dump(&self) -> Vec<String> {
        self.wal.lock().unwrap_or_else(|p| p.into_inner()).dump()
    }

    /// `table_id`のテーブルに`page_id`がまだ属していなければ追加する(第34章)。
    ///
    /// クラッシュ後の`crate::recovery::recover`のRedoが使う。あるページへの
    /// 挿入を表すWALレコードは残っているのに、そのページを`table_id`の
    /// `page_ids`へ登録するはずだった`persist_catalog`の呼び出し
    /// (`Self::attach_page_to_table`)が(カタログページ自体がまだ
    /// `BufferPool`のキャッシュにしか無く)ディスクへ届かないままクラッシュした
    /// 場合、再起動直後のカタログはこのページの存在をまだ知らない。Redoが
    /// 物理的な書き込みを再現しただけでは、この食い違いは直らない。呼び出し側
    /// (`crate::recovery::recover`)は、Redoの全レコードを処理し終えた後に
    /// 1回`persist_catalog`(`Self::persist_catalog_after_recovery`)を呼び、
    /// この関数がその場で溜めた変更をまとめて永続化する。
    pub(crate) fn attach_page_if_missing(&mut self, table_id: TableId, page_id: PageId) -> DbResult<()> {
        let entry = self.table_entry_mut(table_id)?;
        if !entry.page_ids.contains(&page_id) {
            entry.page_ids.push(page_id);
        }
        Ok(())
    }

    fn table_entry_mut(&mut self, table_id: TableId) -> DbResult<&mut TableEntry> {
        self.tables
            .get_mut(&table_id)
            .ok_or_else(|| DbError::TableNotFound(format!("TableId({})", table_id.0)))
    }

    /// Redoが`Self::attach_page_if_missing`で溜めた変更を、まとめて
    /// カタログへ永続化する(第34章、`crate::recovery::recover`専用)。
    pub(crate) fn persist_catalog_after_recovery(&self) -> DbResult<()> {
        self.persist_catalog()
    }

    /// `table_id`の`rid`へ、Redo専用の低レベルな挿入を行う(第34章)。
    ///
    /// `Self::insert`(空きを探す・カタログを更新する通常の挿入)とは違い、
    /// **WALレコードが記録した位置そのもの**(`rid`)へ書き込む。Redoが
    /// 元のトランザクションと寸分違わぬ`RecordId`を再現できるのは、この
    /// 位置ぴったりの書き込みのおかげである。本文「Redoは位置ぴったりに
    /// 書き戻す」を参照。
    ///
    /// `rid.page_id`の現在のPage LSN(`lsn_before`)が`lsn`以上であれば、
    /// この変更はすでにディスクに反映済みなので何もしない(冪等性)。
    /// そうでなければ、`lsn_before`が`Lsn(0)`(このページはWALに追跡された
    /// 変更をまだ一度も受けていない、`crate::page::Page::page_lsn`を参照)かどうかで
    /// `SlottedPage::init`(まっさらな初期化)と`SlottedPage::open`(既存の
    /// 構造の上に追記)を切り替える。この2択が事故なく機能するのは、Redoが
    /// レコードをLSNの昇順で1ページずつ処理し、あるページに対して
    /// スキップした(すでに反映済みの)レコードが常に「LSNが小さい側の
    /// 連続した範囲」になるという、WALの追記専用という性質に支えられた
    /// 前提があるからである(詳しくは本文を参照)。
    pub(crate) fn redo_insert(&mut self, table_id: TableId, rid: RecordId, bytes: &[u8], lsn: Lsn) -> DbResult<()> {
        let lsn_before = self.pool.page_lsn(rid.page_id)?;
        if lsn_before >= lsn {
            return Ok(());
        }
        {
            let mut guard = self.pool.write_page(rid.page_id)?;
            let mut page =
                if lsn_before.0 == 0 { SlottedPage::init(guard.data_mut()) } else { SlottedPage::open(guard.data_mut())? };
            let slot = page.insert(bytes);
            debug_assert_eq!(
                slot,
                Some(rid.slot_id),
                "Redoは元のRecordIdと同じスロットを再現できる前提(本文を参照)"
            );
            let free = page.free_space();
            drop(guard);
            self.fsm.update(rid.page_id, free);
        }
        self.pool.bump_page_lsn(rid.page_id, lsn);
        self.attach_page_if_missing(table_id, rid.page_id)?;
        Ok(())
    }

    /// `rid`のタプルを、Redo専用の低レベルな削除で取り消す(第34章)。
    /// `Self::redo_insert`と同じPage LSNによる冪等性の判定を行う。対象の
    /// ページはこの時点ですでに(以前の`redo_insert`または過去の永続化で)
    /// 初期化済みのはずなので、`SlottedPage::open`だけを使う。
    pub(crate) fn redo_delete(&mut self, rid: RecordId, lsn: Lsn) -> DbResult<()> {
        let lsn_before = self.pool.page_lsn(rid.page_id)?;
        if lsn_before >= lsn {
            return Ok(());
        }
        {
            let mut guard = self.pool.write_page(rid.page_id)?;
            let mut page = SlottedPage::open(guard.data_mut())?;
            page.delete(rid.slot_id);
            let free = page.free_space();
            drop(guard);
            self.fsm.update(rid.page_id, free);
        }
        self.pool.bump_page_lsn(rid.page_id, lsn);
        Ok(())
    }

    /// `rid`のタプルを`bytes`へ置き換える、Redo専用の低レベルな更新
    /// (第34章、同じページ内で完結する`UPDATE`)。`Self::redo_insert`と同じ
    /// Page LSNによる冪等性の判定を行う。
    pub(crate) fn redo_update_in_place(&mut self, rid: RecordId, bytes: &[u8], lsn: Lsn) -> DbResult<()> {
        let lsn_before = self.pool.page_lsn(rid.page_id)?;
        if lsn_before >= lsn {
            return Ok(());
        }
        {
            let mut guard = self.pool.write_page(rid.page_id)?;
            let mut page = SlottedPage::open(guard.data_mut())?;
            page.update(rid.slot_id, bytes);
            let free = page.free_space();
            drop(guard);
            self.fsm.update(rid.page_id, free);
        }
        self.pool.bump_page_lsn(rid.page_id, lsn);
        Ok(())
    }

    /// 現在登録されている全索引を、`table_id`の現在のHeapの中身から作り直す
    /// (第34章、`crate::recovery::recover`専用)。
    ///
    /// 第33章までの限界として、索引ごとの`BufferPool`にはWALを結線して
    /// いない(モジュール冒頭を参照)。索引ページがクラッシュ後にどこまで
    /// 反映されているかを保証する手段が無いため、この章はクラッシュ後の
    /// 索引を個別に修復しようとせず、Heap(WALによって正しく復元済み)から
    /// 全索引を丸ごと作り直す方式を選んだ。`Self::build_index`と同じ手順
    /// (対象列がNULLでない行だけを挿入)で作り直す。`unique`索引で既存行に
    /// 重複キーがあった場合(本来クラッシュ前に検査済みのはずだが、念のため)
    /// は`DbError`をそのまま返す。
    ///
    /// # 索引ファイルの入れ替えは一時ファイル経由(第5部レビュー対応)
    ///
    /// 索引の作り直しは、既存の索引ファイルを直接削除して同じパスへ
    /// 作り直すのではなく、同じディレクトリの**一時ファイル**へ新しい索引を
    /// 書き、その一時ファイルを指す`BTree`を`self.indexes`へ組み込む。
    /// 既存の索引ファイル自体(`index_file_path`が指すパス)は、この時点では
    /// 一切変更しない。
    ///
    /// 戻り値の`Vec<(PathBuf, PathBuf)>`は、`(一時ファイル, 本来の索引
    /// ファイル)`の組であり、呼び出し元(`crate::recovery::recover`)が
    /// Redo・Undo・検証をすべて終えたあとの`Storage::flush`・`sync`と同じ
    /// タイミングで`std::fs::rename`し、初めて既存の索引ファイルを置き換える。
    /// これにより、Redo・Undoの途中で`recover`が失敗しても
    /// (`crate::recovery`モジュールドキュメントの「Undo中のクラッシュへの
    /// 耐性」)、索引ファイルのバイト列も他のファイルと同じく、`recover`を
    /// 呼ぶ直前から一切変わらないままになる。
    /// `self.indexes`が指す`BTree`自体はすでに一時ファイルを指しているため、
    /// この後に続く`Undo`(`apply_wal_undo_disk`が呼ぶ`index_insert_row`・
    /// `index_delete_row`)は、この一時ファイル上のインメモリな`BTree`を
    /// そのまま正しく更新できる。
    pub(crate) fn rebuild_all_indexes_after_recovery(&mut self) -> DbResult<Vec<(PathBuf, PathBuf)>> {
        let index_names: Vec<String> = self.indexes.keys().cloned().collect();
        let mut pending_renames = Vec::with_capacity(index_names.len());
        for index_name in index_names {
            pending_renames.push(self.rebuild_one_index_after_recovery(&index_name)?);
        }
        Ok(pending_renames)
    }

    fn rebuild_one_index_after_recovery(&mut self, index_name: &str) -> DbResult<(PathBuf, PathBuf)> {
        let info = self.indexes.get(index_name).expect("index_namesはself.indexesのキーそのもの").info.clone();
        let index_path = index_file_path(&self.path, index_name);
        let temp_path = index_rebuild_temp_path(&self.path, index_name);
        // 前回の`recover`がここまで進んで失敗した場合、この一時ファイルが
        // 残っている可能性がある。中身は不完全かもしれないので、まっさらな
        // 状態から作り直す(本来の索引ファイルには一度も触れていない)。
        let _ = std::fs::remove_file(&temp_path);
        let disk = DiskManager::open(&temp_path)?;
        let btree = BTree::create(BufferPool::new(disk, DEFAULT_BUFFER_POOL_CAPACITY), info.key_type, info.unique)?;

        let table_info = self.tables.get(&info.table_id).expect("索引の対象テーブルはDROP TABLEされていない前提").info.clone();
        let mut pairs: Vec<(Value, RecordId)> = Vec::new();
        for entry in self.scan(info.table_id)? {
            let (rid, bytes) = entry?;
            let tuple = decode_tuple(&table_info.schema, &bytes)?;
            let value = tuple.get(info.column_index).expect("tupleはschemaと同じ列数を持つ").clone();
            if !value.is_null() {
                pairs.push((value, rid));
            }
        }
        for (value, rid) in &pairs {
            btree.insert(value, *rid)?;
        }
        self.indexes.get_mut(index_name).expect("index_namesはself.indexesのキーそのもの").btree = btree;
        Ok((temp_path, index_path))
    }

    /// `table_id`のテーブルを物理的に回収する(`VACUUM`、第39章)。
    ///
    /// 1. このテーブルの各データページを[`SlottedPage::compact`]し、
    ///    Tombstone化されたタプルの死んだバイト列と、`UPDATE`でサイズが
    ///    変わって取り残された古いバイト列を回収する。
    /// 2. compact後にOccupiedなスロットが1つも残っていないページ
    ///    ([`SlottedPage::is_empty`])は、このテーブルの`page_ids`から外し、
    ///    Free Page Listへ返す(`Self::drop_table`がテーブル削除時に行うのと
    ///    同じ扱い)。
    /// 3. このテーブルに対応する全索引を、[`Self::rebuild_one_index_after_recovery`]
    ///    (第34章、Crash Recoveryが索引を作り直すのと同じ関数)でLazy Delete
    ///    済みのエントリを含まないB+Treeへ作り直す。
    ///
    /// 呼び出し側([`crate::database::Database::execute_vacuum`])は、この
    /// 呼び出しの前にこのテーブルへのExclusiveロックを獲得しておく必要がある
    /// (本文「VACUUMの排他」を参照)。`vacuum_table`自身はロックを取らない。
    ///
    /// 索引の作り直し(手順3)は、`rebuild_one_index_after_recovery`が返す
    /// 一時ファイルを索引ごとに即座に`rename`する。Crash Recovery(第34章)が
    /// 全索引ぶんの`rename`をAnalysis・Redo・Undoの完了後にまとめて行うのとは
    /// 違い、`VACUUM`はクラッシュ安全性を主張しない(本文の限界節を参照)。
    /// 複数の索引を持つテーブルの`VACUUM`中にI/Oエラーが起きた場合、すでに
    /// 作り直し終えた索引と、まだ手つかずの索引が混在した状態で処理が止まる。
    pub fn vacuum_table(&mut self, table_id: TableId) -> DbResult<VacuumReport> {
        let page_ids = self.table_entry(table_id)?.page_ids.clone();
        let mut reclaimed_pages = 0usize;
        let mut remaining = Vec::with_capacity(page_ids.len());
        for page_id in page_ids {
            let mut guard = self.pool.write_page(page_id)?;
            let mut page = SlottedPage::open(guard.data_mut())?;
            page.compact();
            let is_empty = page.is_empty();
            let free = page.free_space();
            drop(guard);
            if is_empty {
                self.fsm.remove(page_id);
                self.free_pages.push(page_id);
                reclaimed_pages += 1;
            } else {
                self.fsm.update(page_id, free);
                remaining.push(page_id);
            }
        }
        self.tables.get_mut(&table_id).expect("直前にtable_entryで存在を確認済み").page_ids = remaining;

        let index_names: Vec<String> =
            self.indexes.values().filter(|e| e.info.table_id == table_id).map(|e| e.info.name.clone()).collect();
        let rebuilt_indexes = index_names.len();
        for index_name in &index_names {
            let (temp_path, index_path) = self.rebuild_one_index_after_recovery(index_name)?;
            std::fs::rename(&temp_path, &index_path)?;
        }

        self.persist_catalog()?;
        Ok(VacuumReport { reclaimed_pages, rebuilt_indexes })
    }

    /// 手動`CHECKPOINT`(第34章)。全dirtyページ(データ・カタログ・索引)を
    /// flush・syncした**あとで**、Checkpointレコード(現在アクティブな
    /// トランザクションの一覧つき)をWALへ書き、そのLSNまで同期する。
    ///
    /// `active`は、この時点でActiveな全トランザクションの`(TransactionId,
    /// wal_last_lsn)`。次回クラッシュ後のAnalysisは、この一覧をTransaction
    /// Tableの初期状態として使うことで、Checkpointより前まで遡ってWALを
    /// 走査せずに済む(本文「Analysisの開始点を短縮する」を参照)。
    ///
    /// flushを先に行うのは、Checkpoint以前の全ページのPage LSNが
    /// Checkpointの時点で確実にこのLSN以上になっている、という前提を
    /// Redoが使えるようにするためである。この前提が無いと、Analysisが
    /// Checkpointより前を読まずに済ませてよい理由が崩れる。
    pub fn checkpoint(&mut self, active: &[(TransactionId, Option<Lsn>)]) -> DbResult<Lsn> {
        self.flush()?;
        self.sync()?;
        let mut w = self.wal.lock().unwrap_or_else(|p| p.into_inner());
        let lsn = w.append_checkpoint(active);
        w.sync()?;
        Ok(lsn)
    }

    /// テーブル名から`TableInfo`を引く。見つからなければ`None`を返す。
    pub fn table(&self, name: &str) -> Option<&TableInfo> {
        self.tables.values().find(|t| t.info.name == name).map(|t| &t.info)
    }

    /// 登録されている全テーブルの`TableInfo`を返す(第27章、`ANALYZE`が
    /// テーブル名を省略した場合に使う)。順序は保証しない。
    pub fn tables(&self) -> impl Iterator<Item = &TableInfo> {
        self.tables.values().map(|t| &t.info)
    }

    /// `table_id`の統計情報(第27章)を引く。`ANALYZE`を一度も実行していなければ
    /// `None`を返す。
    pub fn table_stats(&self, table_id: TableId) -> Option<&TableStats> {
        self.stats.get(&table_id)
    }

    /// `table_id`が現在使っているデータページの枚数(第28章のコストモデルが
    /// Sequential I/Oコストを見積もるために使う)。
    ///
    /// `TableStats::row_count`(第27章)と違い、`ANALYZE`を実行していなくても
    /// 常に実測値を返す。`page_ids`(モジュール冒頭、`TableEntry`)は
    /// `INSERT`・`DELETE`のたびに`Storage`自身が追従させている実データであり、
    /// 統計収集(`ANALYZE`)を経由しない。テーブルが存在しなければ`None`。
    pub fn table_page_count(&self, table_id: TableId) -> Option<u64> {
        self.tables.get(&table_id).map(|entry| entry.page_ids.len() as u64)
    }

    /// `ANALYZE`が集計した`stats`を`table_id`の統計情報として登録し、
    /// Catalogページへ永続化する(第27章)。永続化に失敗した場合は登録を
    /// 取り消す(`Self::create_table`と同じロールバックの方針)。
    pub fn set_table_stats(&mut self, table_id: TableId, stats: TableStats) -> DbResult<()> {
        if let Err(err) = validate_one_table_stats(&self.tables, table_id, &stats) {
            let DbError::CorruptCatalog(detail) = err else {
                unreachable!("validate_one_table_statsは常にCorruptCatalogを返す")
            };
            return Err(DbError::InvalidStats(detail));
        }
        let previous = self.stats.insert(table_id, stats);
        if let Err(err) = self.persist_catalog() {
            match previous {
                Some(previous) => {
                    self.stats.insert(table_id, previous);
                }
                None => {
                    self.stats.remove(&table_id);
                }
            }
            return Err(err);
        }
        Ok(())
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
        // 統計情報(第27章)も、もう存在しないテーブルの分をカタログに残さない。
        self.stats.remove(&id);

        let index_names: Vec<String> =
            self.indexes.values().filter(|e| e.info.table_id == id).map(|e| e.info.name.clone()).collect();
        for index_name in index_names {
            // `drop_index`(SQL経由の入口)ではなく、`is_constraint`による
            // 制限を受けない`drop_index_impl`を直接使う。テーブルごと消す
            // 以上、`PRIMARY KEY`・`UNIQUE`列に対応する制約索引もまとめて
            // 消して当然である(第3部2巡目レビュー対応)。
            self.drop_index_impl(&index_name)?;
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

    /// `table_id`のテーブルの`column_index`番目の列に対応する索引の
    /// [`IndexInfo`]を引く(第25章)。`UNIQUE`かどうかを問わない点が
    /// [`Self::unique_index_for_column`]との違いで、`crate::physical_plan::optimize`が
    /// `WHERE`や`ON`の等値・範囲述語をPoint/Range Index Scan、Index Nested Loop
    /// Joinのアクセスパスとして使えるかどうかを判定するために使う。
    ///
    /// 同じ列に複数の索引(`CREATE INDEX`で作った索引と、`PRIMARY KEY`/`UNIQUE`
    /// 制約から自動生成された索引が両方存在する場合など)があれば、索引名の
    /// 辞書順で最小のものを返す。`self.indexes`は`HashMap`であり走査順は
    /// 非決定的なため、複数候補がある場合に毎回同じ索引を選ぶにはこの
    /// タイブレークが要る。
    pub fn index_for_column(&self, table_id: TableId, column_index: usize) -> Option<&IndexInfo> {
        self.indexes
            .values()
            .filter(|e| e.info.table_id == table_id && e.info.column_index == column_index)
            .map(|e| &e.info)
            .min_by(|a, b| a.name.cmp(&b.name))
    }

    /// 索引名から、その索引の実データを持つ`BTree`を引く(第25章)。
    /// `crate::physical_plan::IndexScanExec`・`IndexNestedLoopJoinExec`が、
    /// `optimize`(または自身の`next()`)が選んだ索引へ`lookup`・`range`する
    /// ために使う。索引名は`optimize`が`Storage::index_for_column`で
    /// 見つけたものをそのまま`PhysicalPlan`に積んでいるため、`None`が返るのは
    /// この関数を`optimize`が選んだのではない索引名で呼んだ場合に限る
    /// (呼び出し側のバグ)。
    pub(crate) fn index_btree(&self, index_name: &str) -> Option<&BTree> {
        self.indexes.get(index_name).map(|e| &e.btree)
    }

    /// `index_name`の索引が1回引かれたことを記録する(第39章)。
    /// `IndexScanExec::new`・`IndexNestedLoopJoinExec::next`(`crate::physical_plan`)が、
    /// `BTree::lookup`・`BTree::range`を呼ぶ直前にそれぞれ1箇所ずつ呼ぶ。
    pub(crate) fn record_index_use(&self, index_name: &str) {
        *self.index_usage.borrow_mut().entry(index_name.to_string()).or_insert(0) += 1;
    }

    /// 登録されている全索引の名前と利用回数を返す(第39章、`SHOW STATS`)。
    /// 一度も引かれていない索引も`0`回として含める(`self.indexes`のキーを
    /// 一次情報にし、`index_usage`はまだ1件も記録の無い索引を欠かすため)。
    /// 順序は保証しない。
    pub fn index_usage_counts(&self) -> Vec<(String, u64)> {
        let usage = self.index_usage.borrow();
        self.indexes.keys().map(|name| (name.clone(), usage.get(name).copied().unwrap_or(0))).collect()
    }

    /// この`Storage`が使う`BufferPool`のヒット/ミス統計を返す(第39章、
    /// `SHOW STATS`)。
    pub fn buffer_pool_stats(&self) -> BufferPoolStats {
        self.pool.stats()
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
    /// [`IndexInfo::primary_key`]が`false`になる。`PRIMARY KEY`・`UNIQUE`列に
    /// 対応する索引は[`Self::create_table_with_constraint_indexes`]が作る。
    pub fn create_index(&mut self, index_name: &str, table_name: &str, column_name: &str, unique: bool) -> DbResult<()> {
        self.create_index_impl(index_name, table_name, column_name, unique, false)
    }

    /// [`Self::create_index`]の実装。カタログへの永続化まで含めて1本の
    /// 索引を作り切る。`CREATE INDEX`(SQL構文)経由の索引は常に
    /// `is_constraint = false`(制約索引ではない)として作る。
    fn create_index_impl(
        &mut self,
        index_name: &str,
        table_name: &str,
        column_name: &str,
        unique: bool,
        primary_key: bool,
    ) -> DbResult<()> {
        self.build_index(index_name, table_name, column_name, unique, primary_key, false)?;
        if let Err(err) = self.persist_catalog() {
            self.indexes.remove(index_name);
            let _ = std::fs::remove_file(index_file_path(&self.path, index_name));
            return Err(err);
        }
        Ok(())
    }

    /// 索引1本を作る処理の本体。索引名の予約・テーブルと列の解決・
    /// Index Build・`self.indexes`への登録までを行うが、カタログへの永続化
    /// (`persist_catalog`)は呼び出し側に任せる。
    ///
    /// [`Self::create_index_impl`](`CREATE INDEX`・単発の制約索引)と
    /// [`Self::create_table_with_constraint_indexes`](`CREATE TABLE`が
    /// まとめて作る複数の制約索引)の両方から使う共通の下請けにしたのは、
    /// 後者が「全索引の準備が整うまで1回も永続化しない」という設計
    /// (第3部2巡目レビュー対応、同メソッドのドキュメントを参照)を採るため、
    /// 永続化のタイミングを索引1本ごとの処理から切り離す必要があったから
    /// である。
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
    /// 索引ファイルを削除する。失敗した場合、`self.indexes`への登録は行わない。
    fn build_index(
        &mut self,
        index_name: &str,
        table_name: &str,
        column_name: &str,
        unique: bool,
        primary_key: bool,
        is_constraint: bool,
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
        let btree = BTree::create(BufferPool::new(disk, DEFAULT_BUFFER_POOL_CAPACITY), key_type, unique)?;

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
            is_constraint,
            key_type,
        };
        self.indexes.insert(index_name.to_string(), IndexEntry { info, btree });
        Ok(())
    }

    /// テーブルを作り、`constraint_columns`(`(列名, PRIMARY KEYかどうか)`の
    /// 並び、`PRIMARY KEY`・`UNIQUE`列に対応する)の制約索引もまとめて作る、
    /// `Database::execute_create_table`(ディスクバックエンド)専用の入口
    /// (第3部2巡目レビュー対応)。
    ///
    /// テーブル単体の`create_table`と、索引単体の`create_constraint_index`を
    /// 順番に呼ぶだけでは、テーブルの永続化(`persist_catalog`)が先に
    /// 完了してしまう。索引名の衝突は事前に検査できても、それ以外の理由
    /// (たとえばテーブル名が長すぎて索引ファイルのパスがOSの上限を超える
    /// I/Oエラーなど)で後続の索引作成が失敗する経路は残るため、
    /// テーブルだけが対応する制約索引を持たずにカタログへ残ってしまう
    /// 余地があった。
    ///
    /// この関数は、テーブルと全ての制約索引の準備([`Self::build_index`]、
    /// 永続化は含まない)が整うまでカタログへの永続化を1回も行わず、
    /// 最後に1回だけ`persist_catalog`する。索引名の衝突・列の解決・
    /// Index Build・カタログの永続化のいずれで失敗しても、テーブル・
    /// 作りかけの索引・索引ファイルのいずれも残さない。
    pub fn create_table_with_constraint_indexes(
        &mut self,
        table_name: &str,
        schema: Schema,
        constraint_columns: &[(String, bool)],
    ) -> DbResult<TableId> {
        if self.tables.values().any(|t| t.info.name == table_name) {
            return Err(DbError::DuplicateTable(table_name.to_string()));
        }
        let index_names: Vec<String> =
            constraint_columns.iter().map(|(column_name, _)| format!("{table_name}_{column_name}_idx")).collect();
        for index_name in &index_names {
            if self.indexes.contains_key(index_name) {
                return Err(DbError::DuplicateIndex(index_name.clone()));
            }
        }

        let id = TableId(self.next_table_id);
        let next_table_id = self.next_table_id.checked_add(1).ok_or(DbError::TableIdSpaceExhausted)?;
        self.next_table_id = next_table_id;
        self.tables.insert(
            id,
            TableEntry { info: TableInfo { id, name: table_name.to_string(), schema }, page_ids: Vec::new() },
        );

        let mut created_index_names: Vec<String> = Vec::new();
        for ((column_name, primary_key), index_name) in constraint_columns.iter().zip(&index_names) {
            if let Err(err) = self.build_index(index_name, table_name, column_name, true, *primary_key, true) {
                self.rollback_table_creation(id, &created_index_names);
                return Err(err);
            }
            created_index_names.push(index_name.clone());
        }

        if let Err(err) = self.persist_catalog() {
            self.rollback_table_creation(id, &created_index_names);
            return Err(err);
        }
        Ok(id)
    }

    /// [`Self::create_table_with_constraint_indexes`]が、テーブルまたは
    /// 制約索引の準備が途中で失敗したときに呼ぶ後始末。`id`のテーブル、
    /// `created_index_names`に集めた(すでに`self.indexes`へ登録済みの)
    /// 索引とその専用ファイルをすべて取り除き、`next_table_id`も呼び出し前の
    /// 値へ戻す。この時点ではまだ一度も`persist_catalog`していないため、
    /// ディスク上のカタログには最初から触れていない。
    fn rollback_table_creation(&mut self, id: TableId, created_index_names: &[String]) {
        self.tables.remove(&id);
        self.next_table_id = id.0;
        for index_name in created_index_names {
            self.indexes.remove(index_name);
            let _ = std::fs::remove_file(index_file_path(&self.path, index_name));
        }
    }

    /// `index_name`の索引を削除する(`DROP INDEX`、第24章)。SQLの
    /// `DROP INDEX`から呼ばれる入口であり、`index_name`が
    /// [`IndexInfo::is_constraint`]な索引(`PRIMARY KEY`・`UNIQUE`列に
    /// 対応して自動生成された索引)を指している場合は
    /// `DbError::CannotDropConstraintIndex`を返して拒否する(第3部2巡目
    /// レビュー対応)。この索引を`DROP INDEX`で消せてしまうと、対応する列の
    /// 一意性制約を検査する手段(`crate::index::check_uniqueness_with_index`)を
    /// 失った状態の`INSERT`・`UPDATE`が、対応する索引が必ずあるという前提
    /// (テーブルは制約を宣言しているのに検査する索引が無い)を崩し、
    /// `DbError::CorruptCatalog`を返すようになってしまう。テーブルを丸ごと
    /// 削除する[`Self::drop_table`]は、この制約索引も一緒に削除する必要が
    /// あるため、この検査を経由しない[`Self::drop_index_impl`]を直接使う。
    ///
    /// 索引が存在しない場合は`DbError::IndexNotFound`を返す。
    pub fn drop_index(&mut self, index_name: &str) -> DbResult<()> {
        let info = self.indexes.get(index_name).ok_or_else(|| DbError::IndexNotFound(index_name.to_string()))?;
        if info.info.is_constraint {
            return Err(DbError::CannotDropConstraintIndex(index_name.to_string()));
        }
        self.drop_index_impl(index_name)
    }

    /// [`Self::drop_index`]・[`Self::drop_table`]が共通して使う、索引1本を
    /// 実際に取り除く処理の本体。[`IndexInfo::is_constraint`]による制限を
    /// 一切受けない、テーブル削除用の内部経路である。
    ///
    /// `pub`ではなく`pub(crate)`にとどめてあるのは、SQLの`DROP INDEX`
    /// (`Database::execute_drop_index`)には必ず[`Self::drop_index`]
    /// (制約索引を拒否する)を経由させ、この関数へは`crate`内部からしか
    /// 到達できないようにするためである。テストコード(`crate::index`の
    /// 回帰テストなど)が「制約索引だけが欠落した」壊れた状態を意図的に
    /// 再現する際にも、この関数を使う。
    ///
    /// 索引が存在しない場合は`DbError::IndexNotFound`を返す。カタログからの
    /// 削除に成功したら、その索引専用のファイル([`index_file_path`])を
    /// 削除する。ファイルの削除は`drop_table`(第15章)がテーブルのページを
    /// 即座にはファイルから取り除かない(Free Page Listへ積むだけ)のとは違い、
    /// 索引は他のどのテーブル・索引ともページを共有しない専用ファイルなので、
    /// そのまま`std::fs::remove_file`できる。
    pub(crate) fn drop_index_impl(&mut self, index_name: &str) -> DbResult<()> {
        if self.indexes.remove(index_name).is_none() {
            return Err(DbError::IndexNotFound(index_name.to_string()));
        }
        // 利用回数(第39章)も、もう存在しない索引の分を残さない。
        self.index_usage.borrow_mut().remove(index_name);
        self.persist_catalog()?;
        let index_path = index_file_path(&self.path, index_name);
        std::fs::remove_file(&index_path)?;
        Ok(())
    }

    /// `table_id`の全索引について、`tuple`を挿入(または`UPDATE`で書き直す)
    /// 際にどの索引にもキーが収まることを、Heapへの書き込みより前に確認する
    /// (第3部レビュー対応)。
    ///
    /// `crate::executor::storage_insert`・`storage_update`は、`storage.insert`・
    /// `storage.update`でHeapを書き換える前に、この検査を全対象行に対して
    /// 済ませておく。これを怠ると、Heapへの書き込みが終わった**後**に
    /// `index_insert_row`が`DbError::BTreeKeyTooLarge`で失敗し、Heapには
    /// 存在するが索引には無い行(Seq Scanでは見えるがIndex Scanでは見えない
    /// 行)が残ってしまう。
    ///
    /// この検査は`crate::btree::BTree::check_key_fits`を全索引に対して行う。
    /// `BTree::insert`は、キー長が[`crate::btree::BTree`]モジュール
    /// ドキュメントの「Split中の伝播が安全である理由」で説明する上限を
    /// 超えていない限り、多段のLeaf・Internal Splitのどの階層でも
    /// `DbError::BTreeKeyTooLarge`を返さないことが構造的に保証されている
    /// (第3部2巡目レビュー対応)。`check_key_fits`はその上限と同じ基準で
    /// 判定するため、この検査を通過した`tuple`に対する`index_insert_row`の
    /// 呼び出しは、(`BufferPool`自体のI/Oエラーのような無関係な理由を除けば)
    /// `DbError::BTreeKeyTooLarge`では失敗しない。
    pub fn check_indexes_accept_row(&self, table_id: TableId, tuple: &Tuple) -> DbResult<()> {
        for entry in self.indexes.values().filter(|e| e.info.table_id == table_id) {
            let Some(value) = tuple.get(entry.info.column_index) else { continue };
            if value.is_null() {
                continue;
            }
            entry
                .btree
                .check_key_fits(value)
                .map_err(|err| translate_btree_error(err, entry.info.primary_key, &entry.info.column_name, value))?;
        }
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
    /// この関数を呼ぶ前に`crate::index::check_uniqueness_with_index`・
    /// [`Self::check_indexes_accept_row`]で検査を終えている前提のため、通常は
    /// ここで初めて違反が見つかることはない。
    ///
    /// # 第3部レビュー対応: 途中の索引が失敗したら、それより前の索引を戻す
    ///
    /// `table_id`が複数の索引を持つ場合、この関数はそれらを1つずつ順に
    /// `insert`していく。[`Self::check_indexes_accept_row`]を通過していれば、
    /// 個々の`BTree::insert`は`DbError::BTreeKeyTooLarge`では失敗しない
    /// ([`Self::check_indexes_accept_row`]のドキュメントを参照)ため、
    /// 途中の索引が失敗する経路は理論上残っていない。それでもこの巻き戻しを
    /// 残してあるのは、`check_indexes_accept_row`の呼び出しを怠った場合や、
    /// `BufferPool`のI/Oエラーのような`BTreeKeyTooLarge`以外の理由で
    /// 個々の`insert`が失敗した場合の保険である。失敗したとき、それより前に
    /// すでに`insert`済みだった索引をそのままにしてエラーを返すと、Heap
    /// (この行自体はまだ存在する)・一部の索引(この行を指す)・残りの索引
    /// (この行を指さない)が食い違ったままになる。この関数はそれを避け、
    /// 失敗した索引より前に成功していた`insert`を逆順に`delete`で
    /// 巻き戻してからエラーを返す。それでもHeap自体(この`rid`の行)は
    /// この関数の責務の外にあるため戻さない。呼び出し側
    /// (`crate::executor::storage_insert`・`storage_update`)が、この関数が
    /// 返したエラーを見てHeap側の巻き戻しを行う。
    ///
    /// # 第3部2巡目レビュー対応: 失敗した索引自身に反映が残る問題も解消済み
    ///
    /// この関数がまだ`BTree::insert`自体の多段Split伝播の途中でエラーに
    /// なりうると仮定した初期の設計では、失敗した(まさに今`insert`しようと
    /// していた)そのB+Tree自身の中に、下位のSplitだけがすでに反映された
    /// 部分的な状態が残る余地があった。この状態は`insert`(呼び出し元の
    /// `BTree`)からは失敗として観測されるが、`next_leaf`のリンクを辿る
    /// `lookup`・`range`からは(部分的に)見えてしまうため、この関数の
    /// 巻き戻し(失敗した索引より**前**の索引を戻すだけ)では手当てできない
    /// 種類の不整合だった。`crate::btree::BTree::insert`が多段伝播全体を
    /// 原子的に扱うよう修正された(モジュールドキュメントの「Split中の
    /// 伝播が安全である理由」を参照)ことで、個々の`BTree::insert`呼び出しは
    /// 「完全に成功する」か「呼び出す前のそのB+Treeを一切変更せず失敗する」
    /// かのどちらかしかなくなり、この問題自体が構造的に起こらなくなった。
    pub fn index_insert_row(&mut self, table_id: TableId, tuple: &Tuple, rid: RecordId) -> DbResult<()> {
        let index_names: Vec<String> =
            self.indexes.values().filter(|e| e.info.table_id == table_id).map(|e| e.info.name.clone()).collect();

        let mut applied: Vec<(String, crate::types::Value)> = Vec::new();
        for index_name in index_names {
            let entry = self.indexes.get_mut(&index_name).expect("直前にこのテーブルの索引として集めた名前なので必ず存在する");
            let Some(value) = tuple.get(entry.info.column_index).cloned() else { continue };
            if value.is_null() {
                continue;
            }
            match entry.btree.insert(&value, rid) {
                Ok(()) => applied.push((index_name, value)),
                Err(err) => {
                    let (primary_key, column_name) = (entry.info.primary_key, entry.info.column_name.clone());
                    for (applied_name, applied_value) in applied.into_iter().rev() {
                        if let Some(applied_entry) = self.indexes.get_mut(&applied_name) {
                            let _ = applied_entry.btree.delete(&applied_value, rid);
                        }
                    }
                    return Err(translate_btree_error(err, primary_key, &column_name, &value));
                }
            }
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
        let bytes = encode_catalog(self.next_table_id, &self.tables, &self.free_pages, &index_infos, &self.stats);
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

/// 統計情報(`crate::statistics::TableStats`)が、対応するテーブル定義と
/// 意味的に整合しているかを検査する(第4部レビュー対応)。
///
/// `decode_catalog`は統計に紐づく`TableId`の重複だけを検査し、`Storage::open`が
/// 呼ぶ`validate_table_metadata`もテーブル定義自体の整合性(名前の重複など)
/// しか見ない。どちらも、統計情報の**中身**がテーブル定義や自分自身と矛盾して
/// いないかは検査していなかった。この関数は`Storage::open`(カタログを復元
/// する経路)と`Storage::set_table_stats`(`ANALYZE`が新しい統計を登録する
/// 経路)の両方から呼び、次を検査する。
///
/// * 統計に紐づく`TableId`が実在するテーブルを指しているか。
/// * 列数・列の型が、対応するテーブル定義の`Schema`と一致するか。
/// * `null_count <= row_count`、`distinct_count <= 非NULL行数`。
/// * 非NULL行数が0であることと`distinct_count`が0であることが同値であること
///   (非NULL行が無いのに`distinct_count > 0`、または非NULL行があるのに
///   `distinct_count == 0`という不整合を防ぐ、第4部2巡目レビュー対応)。
/// * `distinct_count >= mcv.len()`(MCVは全体のDistinct値の部分集合)。
/// * MCVの件数が上限([`MCV_MAX_ENTRIES`])以下で、値が重複せず、値の型が
///   列の型と一致し、出現回数が`0`より大きく非NULL行数以下であり、出現回数が
///   降順(`crate::statistics::ColumnStats::mcv`の契約)に並んでいること。
/// * Histogramのバケツ数が上限([`HISTOGRAM_BUCKET_COUNT`])以下で、各バケツの
///   境界の型が列の型と一致し、`lower <= upper`であり、隣接するバケツどうしが
///   `前のバケツのupper < 次のバケツのlower`という**厳密な**昇順(`<=`ではなく
///   `<`)に並んでいること。`crate::estimator::equality_selectivity_within_non_null`
///   は「同じ値を含むHistogramバケツは必ず1個」という不変条件に依存しており
///   (`crate::statistics`モジュールの説明を参照)、`previous_upper ==
///   bucket.lower`(隣接バケツが境界の値を共有する)を許すと、その値の行が
///   2つのバケツに分かれて数えられ、選択率を過小評価してしまう(第4部4巡目
///   レビュー対応。値`1`を1行ずつ持つ同一境界の2バケツを許した場合、
///   `v = 1`の選択率が期待値`0.10`に対して`0.05`になる再現がある)。
/// * MCV・Histogramの値・境界がすべて`Min`/`Max`の範囲に収まっていること。
/// * MCVの出現回数の合計とHistogramのバケツ行数の合計を足すと、ちょうど
///   非NULL行数(`row_count - null_count`)に一致すること
///   (`u64`の加算オーバーフローは`checked_add`で検出する)。
///
/// いずれかに違反する場合は`DbError::CorruptCatalog`を返す(`Storage::open`が
/// 復元したカタログ全体に対して呼ぶ)。`Storage::set_table_stats`は、新しく
/// 登録しようとしている1テーブルぶんだけを検査する[`validate_one_table_stats`]
/// を使い、違反を`DbError::InvalidStats`として返す(モジュール冒頭の説明、
/// および`DbError::InvalidStats`のドキュメントを参照)。
///
/// `previous_upper < bucket.lower`という検査を追加する前は、隣接バケツが
/// 境界の値を共有する統計(このブランチの開発途中、Histogramが同値の
/// 連続runをバケツ境界で分割していた時期のコミットでのみ生成されえた形式、
/// 第4部3巡目レビュー対応より前)も、構文的には妥当なCatalogページとして
/// 永続化・再オープンできてしまっていた。この検査により、そのような
/// カタログを再オープンしようとすると`DbError::CorruptCatalog`として
/// 決定的に拒否されるようになる。章をまたいだファイル互換性を約束しない
/// という、このモジュールが一貫して採っている方針(モジュール冒頭を参照)の
/// 範囲内の変更である。
fn validate_stats_metadata(tables: &HashMap<TableId, TableEntry>, stats: &HashMap<TableId, TableStats>) -> DbResult<()> {
    for (&table_id, table_stats) in stats {
        validate_one_table_stats(tables, table_id, table_stats)?;
    }
    Ok(())
}

/// [`validate_stats_metadata`]が1テーブルぶんに対して行う検査。戻り値は常に
/// `DbError::CorruptCatalog`で、呼び出し側([`Storage::set_table_stats`])が
/// 必要に応じて`DbError::InvalidStats`へ読み替える。
fn validate_one_table_stats(tables: &HashMap<TableId, TableEntry>, table_id: TableId, table_stats: &TableStats) -> DbResult<()> {
    let entry = tables
        .get(&table_id)
        .ok_or_else(|| DbError::CorruptCatalog(format!("統計情報のTableId({})に対応するテーブル定義がありません", table_id.0)))?;
    let columns = entry.info.schema.columns();
    if table_stats.columns.len() != columns.len() {
        return Err(DbError::CorruptCatalog(format!(
            "TableId({})の統計情報の列数({})がテーブル定義の列数({})と一致しません",
            table_id.0,
            table_stats.columns.len(),
            columns.len()
        )));
    }
    for (column, column_stats) in columns.iter().zip(&table_stats.columns) {
        validate_column_stats_metadata(table_id.0, &column.name, column.data_type, table_stats.row_count, column_stats)?;
    }
    Ok(())
}

/// [`validate_stats_metadata`]が列1個ぶんに対して行う検査。
fn validate_column_stats_metadata(
    table_id: u64,
    column_name: &str,
    data_type: DataType,
    row_count: u64,
    stats: &ColumnStats,
) -> DbResult<()> {
    let corrupt = |detail: String| {
        DbError::CorruptCatalog(format!("TableId({table_id})の列'{column_name}'の統計情報が不正です: {detail}"))
    };

    if stats.null_count > row_count {
        return Err(corrupt(format!("null_count({})がrow_count({row_count})を超えています", stats.null_count)));
    }
    let non_null_rows = row_count - stats.null_count;
    if stats.distinct_count > non_null_rows {
        return Err(corrupt(format!("distinct_count({})が非NULL行数({non_null_rows})を超えています", stats.distinct_count)));
    }
    // `non_null_rows == 0`(非NULLの値が1件も無い)なら、Distinct値も1つも
    // 無いはずである。逆に`non_null_rows > 0`なら、少なくとも1個は
    // Distinct値があるはずである。この同値性を検査しないと、非NULL行が
    // あるのに`distinct_count = 0`という統計(後段の
    // `crate::estimator`の`saturating_sub(...).max(1)`という底上げが、この
    // 意味的な不整合を数値上隠してしまう)を受理してしまう(第4部2巡目
    // レビュー対応)。
    if (non_null_rows == 0) != (stats.distinct_count == 0) {
        return Err(corrupt(format!(
            "非NULL行数({non_null_rows})とdistinct_count({})の0/非0が一致しません",
            stats.distinct_count
        )));
    }
    // MCVはDistinct値の部分集合である以上、その件数が全体のDistinct値数を
    // 超えることはありえない。
    if stats.distinct_count < stats.mcv.len() as u64 {
        return Err(corrupt(format!("distinct_count({})がMCVの件数({})を下回っています", stats.distinct_count, stats.mcv.len())));
    }

    if stats.min.is_some() != stats.max.is_some() {
        return Err(corrupt("MinとMaxの有無が一致しません(非NULLの値が無ければ両方None、あれば両方Someのはず)".to_string()));
    }
    for value in stats.min.iter().chain(stats.max.iter()) {
        if !value_matches_type(value, data_type) {
            return Err(corrupt(format!("Min/Maxの値の型が列の型({data_type:?})と一致しません: {value:?}")));
        }
    }
    if let (Some(min), Some(max)) = (&stats.min, &stats.max)
        && compare_values(min, max) == Ordering::Greater
    {
        return Err(corrupt(format!("Min({min:?})がMax({max:?})より大きいです")));
    }
    let within_min_max = |value: &Value| -> bool {
        match (&stats.min, &stats.max) {
            (Some(min), Some(max)) => compare_values(value, min) != Ordering::Less && compare_values(value, max) != Ordering::Greater,
            _ => false,
        }
    };

    if stats.mcv.len() > MCV_MAX_ENTRIES {
        return Err(corrupt(format!("MCVの件数({})が上限({MCV_MAX_ENTRIES})を超えています", stats.mcv.len())));
    }
    let mut seen_mcv_values = HashSet::new();
    let mut mcv_row_total: u64 = 0;
    let mut previous_mcv_count: Option<u64> = None;
    for (value, count) in &stats.mcv {
        if !value_matches_type(value, data_type) {
            return Err(corrupt(format!("MCVの値の型が列の型({data_type:?})と一致しません: {value:?}")));
        }
        if !within_min_max(value) {
            return Err(corrupt(format!("MCVの値がMin/Maxの範囲外です: {value:?}")));
        }
        if !seen_mcv_values.insert(value) {
            return Err(corrupt(format!("MCVに同じ値が複数回出現しています: {value:?}")));
        }
        if *count == 0 || *count > non_null_rows {
            return Err(corrupt(format!("MCVの出現回数({count})が非NULL行数({non_null_rows})の範囲外です")));
        }
        // `ColumnStats::mcv`は出現回数の降順であることを契約として文書化して
        // いる(`crate::statistics::ColumnStats::mcv`のドキュメントコメント)。
        // `crate::statistics::extract_mcv`はこの順序で組み立てるが、永続化
        // されたバイト列や`Storage::set_table_stats`への外部入力はこの契約を
        // 経由しないため、ここで検査して不変条件へ昇格させる(第4部2巡目
        // レビュー対応)。
        if let Some(previous_mcv_count) = previous_mcv_count
            && *count > previous_mcv_count
        {
            return Err(corrupt(format!("MCVが出現回数の降順になっていません: {previous_mcv_count} の次に {count}")));
        }
        previous_mcv_count = Some(*count);
        mcv_row_total =
            mcv_row_total.checked_add(*count).ok_or_else(|| corrupt("MCVの出現回数の合計がu64の範囲を超えます".to_string()))?;
    }

    if stats.histogram.len() > HISTOGRAM_BUCKET_COUNT {
        return Err(corrupt(format!("Histogramのバケツ数({})が上限({HISTOGRAM_BUCKET_COUNT})を超えています", stats.histogram.len())));
    }
    let mut histogram_row_total: u64 = 0;
    let mut previous_upper: Option<&Value> = None;
    for bucket in &stats.histogram {
        if !value_matches_type(&bucket.lower, data_type) || !value_matches_type(&bucket.upper, data_type) {
            return Err(corrupt("Histogramのバケツ境界の型が列の型と一致しません".to_string()));
        }
        if compare_values(&bucket.lower, &bucket.upper) == Ordering::Greater {
            return Err(corrupt(format!("バケツのlower({:?})がupper({:?})より大きいです", bucket.lower, bucket.upper)));
        }
        if !within_min_max(&bucket.lower) || !within_min_max(&bucket.upper) {
            return Err(corrupt("Histogramのバケツ境界がMin/Maxの範囲外です".to_string()));
        }
        // `previous_upper < bucket.lower`という**厳密な**分離を要求する
        // (`<=`ではない)。`crate::estimator::equality_selectivity_within_non_null`
        // は「同じ値を含むHistogramバケツは必ず1個」という不変条件に依存して
        // おり(`crate::statistics`モジュールの説明を参照)、隣接バケツの
        // 境界が`previous_upper == bucket.lower`(同じ値を共有する)ことを
        // 許すと、その値の行の一部が前のバケツに、残りが次のバケツに
        // 分かれて数えられてしまう。値`1`を1行ずつ持つ同一境界の2バケツ
        // (`[..., 1]`と`[1, ...]`)を許してしまうと、`v = 1`の選択率は
        // 見つかった最初のバケツの1行分だけを見て見積もることになり、
        // 実際の2行の半分(0.05 対 期待値0.10)になる(第4部4巡目レビュー
        // 対応)。
        if let Some(previous_upper) = previous_upper
            && compare_values(previous_upper, &bucket.lower) != Ordering::Less
        {
            return Err(corrupt("Histogramのバケツが厳密な昇順(前のバケツのupperより大きいlower)に並んでいません".to_string()));
        }
        if bucket.row_count == 0 {
            return Err(corrupt("Histogramのバケツのrow_countが0です".to_string()));
        }
        histogram_row_total = histogram_row_total
            .checked_add(bucket.row_count)
            .ok_or_else(|| corrupt("Histogramのバケツのrow_countの合計がu64の範囲を超えます".to_string()))?;
        previous_upper = Some(&bucket.upper);
    }

    let total_non_null = mcv_row_total
        .checked_add(histogram_row_total)
        .ok_or_else(|| corrupt("MCVとHistogramの行数合計がu64の範囲を超えます".to_string()))?;
    if total_non_null != non_null_rows {
        return Err(corrupt(format!(
            "MCVとHistogramの行数合計({total_non_null})が非NULL行数({non_null_rows})と一致しません"
        )));
    }

    Ok(())
}

/// `value`が`data_type`の列に収まる型かどうか。`Value::Null`はここには渡って
/// こない前提(`Min`/`Max`は`None`で「値が無い」を表し、MCV・Histogramの
/// 境界は非NULL値だけを持つ)なので、`Value::Null`は常に不一致として扱う。
fn value_matches_type(value: &Value, data_type: DataType) -> bool {
    matches!(
        (value, data_type),
        (Value::Boolean(_), DataType::Boolean) | (Value::BigInt(_), DataType::BigInt) | (Value::Text(_), DataType::Text)
    )
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

/// Catalogページのpayloadの先頭に置く、固定の識別バイト列(第3部レビュー
/// 対応)。
///
/// この章より前のカタログレイアウト(`is_constraint`を持たない版など)は、
/// payloadの先頭がこの8バイトと一致することはまず無い(先頭は
/// `next_table_id`という`u64`の値であり、任意のテーブル数を表しうるが、
/// この8バイトのASCII文字列と偶然一致する確率は無視できる)。
/// `decode_catalog`はまずこの8バイトを検査し、一致しなければ
/// `DbError::CorruptCatalog`で即座に拒否する。これにより、レイアウトが
/// 変わった後に古いカタログを新しいコードで開いても、フィールドを
/// 読み違えたまま「たまたま妥当に見える値」を受理してしまうことがない。
const CATALOG_MAGIC: [u8; 8] = *b"MDBCTLG1";

/// Catalogページのレイアウト版(第3部レビュー対応)。
///
/// [`CATALOG_MAGIC`]の直後に置く`u32`で、`decode_catalog`は
/// [`CATALOG_LAYOUT_VERSION`]と完全に一致する場合だけ、それ以降のバイト列を
/// このモジュールの現在の`encode_catalog`と同じレイアウトとして解釈する。
///
/// **索引メタデータ・テーブル定義・Free Page Listのいずれかのレイアウトを
/// 変更する(フィールドの追加・削除・並び替え、型の変更など)たびに、
/// この定数を1つ増やすこと。** 増やし忘れると、新しいコードが古いレイアウトの
/// バイト列を新しいレイアウトとして読み違え、フィールドの境界がずれた
/// まま「たまたま妥当に見える値」を受理してしまう危険がある
/// (`is_constraint`フィールドを追加した際に実際に起きた不具合)。
/// 第27章で統計情報セクションを追加した際、`1`から`2`へ上げた。
const CATALOG_LAYOUT_VERSION: u32 = 3;

/// `decode_catalog`が返す、Catalogページから復元した状態。
struct DecodedCatalog {
    next_table_id: u64,
    tables: HashMap<TableId, TableEntry>,
    free_pages: Vec<PageId>,
    /// 索引メタデータ(第24章)。索引名の重複が無いことは`decode_catalog`が
    /// `Vec`へ積む時点で検査済み。
    indexes: Vec<IndexInfo>,
    /// 統計情報(第27章)。`ANALYZE`を実行していないテーブルはここに現れない。
    stats: HashMap<TableId, TableStats>,
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
    stats: &HashMap<TableId, TableStats>,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&CATALOG_MAGIC);
    out.extend_from_slice(&CATALOG_LAYOUT_VERSION.to_le_bytes());
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
        out.push(u8::from(info.is_constraint));
    }

    let mut sorted_stats: Vec<(&TableId, &TableStats)> = stats.iter().collect();
    sorted_stats.sort_by_key(|(id, _)| id.0);
    out.extend_from_slice(&(sorted_stats.len() as u32).to_le_bytes());
    for (table_id, table_stats) in sorted_stats {
        out.extend_from_slice(&table_id.0.to_le_bytes());
        out.extend_from_slice(&table_stats.row_count.to_le_bytes());
        out.extend_from_slice(&(table_stats.columns.len() as u16).to_le_bytes());
        for column in &table_stats.columns {
            out.extend_from_slice(&column.null_count.to_le_bytes());
            out.extend_from_slice(&column.distinct_count.to_le_bytes());
            encode_optional_value(&column.min, &mut out);
            encode_optional_value(&column.max, &mut out);
            out.extend_from_slice(&(column.mcv.len() as u16).to_le_bytes());
            for (value, count) in &column.mcv {
                encode_value(value, &mut out);
                out.extend_from_slice(&count.to_le_bytes());
            }
            out.extend_from_slice(&(column.histogram.len() as u16).to_le_bytes());
            for bucket in &column.histogram {
                encode_value(&bucket.lower, &mut out);
                encode_value(&bucket.upper, &mut out);
                out.extend_from_slice(&bucket.row_count.to_le_bytes());
            }
        }
    }

    out
}

/// [`Value`]をタグ(0=NULL, 1=BOOLEAN, 2=BIGINT, 3=TEXT)+ペイロードへ
/// エンコードする(第27章)。既存の`data_type_to_u8`(列の型だけを表す)とは
/// 異なり、値そのものを復元できる自己記述形式にする必要がある
/// (統計情報のMin/Max/Histogram境界は値そのものだから)。
fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0),
        Value::Boolean(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Value::BigInt(n) => {
            out.push(2);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Text(s) => {
            out.push(3);
            let bytes = s.as_bytes();
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(bytes);
        }
    }
}

/// `Option<Value>`をエンコードする(第27章)。`StatsCollector`は`NULL`値を
/// `min`/`max`の対象に含めないため、`None`(値が1件も無い)と`Value::Null`が
/// 同時に起こることは無い。この不変条件により、`None`を`Value::Null`と
/// 同じタグ(0)で表せば、専用の存在フラグを別に持つ必要が無い
/// (モジュール冒頭のレイアウト解説を参照)。
fn encode_optional_value(value: &Option<Value>, out: &mut Vec<u8>) {
    match value {
        Some(value) => encode_value(value, out),
        None => encode_value(&Value::Null, out),
    }
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

    let magic = take(&mut cursor, CATALOG_MAGIC.len(), "catalog_magic")?;
    if magic != CATALOG_MAGIC {
        return Err(DbError::CorruptCatalog(
            "Catalogページの先頭が識別バイト列と一致しません(このレイアウトを導入する前の、\
             章をまたいで互換性の無い古いカタログである可能性があります)"
                .to_string(),
        ));
    }
    let layout_version = take_u32(&mut cursor, "catalog_layout_version")?;
    if layout_version != CATALOG_LAYOUT_VERSION {
        return Err(DbError::CorruptCatalog(format!(
            "Catalogページのレイアウト版が不明です: {layout_version}(現在のコードは{CATALOG_LAYOUT_VERSION}のみ理解します)"
        )));
    }

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
        let is_constraint = take_bool(&mut cursor, "索引のis_constraint")?;

        if !seen_index_names.insert(name.clone()) {
            // encode_catalogが索引名の一意性を保証しているHashMap<String, _>を
            // 経由していれば起こらないが、`decode_catalog`は入力を信用しない
            // (テーブル名の重複検出=`TableId`の重複検出と同じ理由)。
            return Err(DbError::CorruptCatalog(format!("索引名'{name}'が複数回出現しています")));
        }
        indexes.push(IndexInfo { name, table_id, column_index, column_name, unique, primary_key, is_constraint, key_type });
    }

    let stats_count = take_u32(&mut cursor, "stats_count")? as usize;
    let mut stats = HashMap::new();
    for _ in 0..stats_count {
        let table_id = TableId(take_u64(&mut cursor, "統計のtable_id")?);
        let row_count = take_u64(&mut cursor, "統計のrow_count")?;
        let column_count = take_u16(&mut cursor, "統計のcolumn_count")? as usize;
        let mut columns = Vec::new();
        for _ in 0..column_count {
            let null_count = take_u64(&mut cursor, "統計のnull_count")?;
            let distinct_count = take_u64(&mut cursor, "統計のdistinct_count")?;
            let min = decode_optional_value(&mut cursor)?;
            let max = decode_optional_value(&mut cursor)?;
            let mcv_count = take_u16(&mut cursor, "統計のmcv_count")? as usize;
            let mut mcv = Vec::new();
            for _ in 0..mcv_count {
                let value = decode_value(&mut cursor)?;
                let count = take_u64(&mut cursor, "統計のmcvのcount")?;
                mcv.push((value, count));
            }
            let bucket_count = take_u16(&mut cursor, "統計のbucket_count")? as usize;
            let mut histogram = Vec::new();
            for _ in 0..bucket_count {
                let lower = decode_value(&mut cursor)?;
                let upper = decode_value(&mut cursor)?;
                let bucket_row_count = take_u64(&mut cursor, "統計のバケツのrow_count")?;
                histogram.push(Bucket { lower, upper, row_count: bucket_row_count });
            }
            columns.push(ColumnStats { null_count, distinct_count, min, max, mcv, histogram });
        }
        if stats.insert(table_id, TableStats { row_count, columns }).is_some() {
            return Err(DbError::CorruptCatalog(format!("TableId({})の統計情報が複数回出現しています", table_id.0)));
        }
    }

    Ok(DecodedCatalog {
        next_table_id,
        tables,
        free_pages,
        indexes,
        stats,
    })
}

/// [`encode_value`]の対。
fn decode_value(cursor: &mut &[u8]) -> DbResult<Value> {
    let tag = take_u8(cursor, "value_tag")?;
    match tag {
        0 => Ok(Value::Null),
        1 => Ok(Value::Boolean(take_bool(cursor, "value_bool")?)),
        2 => Ok(Value::BigInt(i64::from_le_bytes(take(cursor, 8, "value_bigint")?.try_into().unwrap()))),
        3 => {
            let len = take_u32(cursor, "value_text_len")? as usize;
            Ok(Value::Text(take_string(cursor, len, "value_text")?))
        }
        other => Err(DbError::CorruptCatalog(format!("未知のValueタグです: {other}"))),
    }
}

/// [`encode_optional_value`]の対。タグ0(NULL)を`None`として復元する
/// (モジュール冒頭のレイアウト解説を参照)。
fn decode_optional_value(cursor: &mut &[u8]) -> DbResult<Option<Value>> {
    match decode_value(cursor)? {
        Value::Null => Ok(None),
        other => Ok(Some(other)),
    }
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
        payload[0..8].copy_from_slice(&CATALOG_MAGIC);
        payload[8..12].copy_from_slice(&CATALOG_LAYOUT_VERSION.to_le_bytes());
        payload[12..20].copy_from_slice(&0u64.to_le_bytes()); // next_table_id
        payload[20..24].copy_from_slice(&1u32.to_le_bytes()); // table_count = 1
        payload[24..28].copy_from_slice(&0u32.to_le_bytes()); // free_page_count
        payload[28..36].copy_from_slice(&0u64.to_le_bytes()); // table_id
        // name_lenを、ページに残っている実バイト数よりずっと大きい値へ偽る。
        payload[36..38].copy_from_slice(&u16::MAX.to_le_bytes());

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

    // ------------------------------------------------------------------
    // validate_stats_metadata(第4部レビュー対応)
    // ------------------------------------------------------------------

    fn one_bigint_schema() -> Schema {
        Schema::new(vec![Column::new("v", DataType::BigInt, true)])
    }

    /// テストが違反させたい1点だけを変えられる、境界値として妥当な
    /// `ColumnStats`(100行、うち10行が非NULLで0〜9の1回ずつ、10バケツ)。
    fn valid_column_stats() -> ColumnStats {
        ColumnStats {
            null_count: 90,
            distinct_count: 10,
            min: Some(Value::BigInt(0)),
            max: Some(Value::BigInt(9)),
            mcv: Vec::new(),
            histogram: (0..10).map(|v| Bucket { lower: Value::BigInt(v), upper: Value::BigInt(v), row_count: 1 }).collect(),
        }
    }

    fn valid_table_stats() -> TableStats {
        TableStats { row_count: 100, columns: vec![valid_column_stats()] }
    }

    #[test]
    fn set_table_stats_accepts_a_boundary_valid_column() {
        let path = temp_path("stats-valid");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        storage.set_table_stats(table_id, valid_table_stats()).unwrap();
        assert_eq!(storage.table_stats(table_id).unwrap().row_count, 100);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_an_unknown_table_id() {
        let path = temp_path("stats-unknown-table");
        let mut storage = Storage::create(&path).unwrap();
        let err = expect_err(storage.set_table_stats(TableId(999), valid_table_stats()));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_a_column_count_mismatch() {
        let path = temp_path("stats-column-count");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let stats = TableStats { row_count: 100, columns: vec![valid_column_stats(), valid_column_stats()] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_null_count_exceeding_row_count() {
        let path = temp_path("stats-null-count");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.null_count = 101; // row_count(100)を超える
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_distinct_count_exceeding_non_null_rows() {
        let path = temp_path("stats-distinct-count");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.distinct_count = 11; // 非NULL行数(10)を超える
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_distinct_count_zero_when_non_null_rows_exist() {
        // 非NULL行(10行)があるのにdistinct_count=0は、後段の
        // `saturating_sub(...).max(1)`という底上げにこの不整合を隠されてしまう
        // (第4部2巡目レビュー対応)。
        let path = temp_path("stats-distinct-zero");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.distinct_count = 0;
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_distinct_count_positive_when_no_non_null_rows_exist() {
        // 非NULL行が1件も無いのにdistinct_count > 0という、逆方向の不整合。
        let path = temp_path("stats-distinct-positive");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let column = ColumnStats { null_count: 100, distinct_count: 1, min: None, max: None, mcv: Vec::new(), histogram: Vec::new() };
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_distinct_count_below_mcv_len() {
        // MCVは全体のDistinct値の部分集合であるため、distinct_countがMCVの
        // 件数を下回ることはありえない。
        let path = temp_path("stats-distinct-below-mcv");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.distinct_count = 1;
        column.mcv = vec![(Value::BigInt(0), 5), (Value::BigInt(1), 3)];
        column.histogram = vec![Bucket { lower: Value::BigInt(2), upper: Value::BigInt(9), row_count: 2 }];
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_mcv_not_sorted_by_count_descending() {
        let path = temp_path("stats-mcv-order");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.distinct_count = 2;
        column.mcv = vec![(Value::BigInt(0), 3), (Value::BigInt(1), 5)]; // 昇順(不正)
        column.histogram = Vec::new();
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_catalog_whose_distinct_count_is_zero_despite_non_null_rows() {
        // set_table_statsは常にvalidate_one_table_statsを通すため、この不整合は
        // `open_rejects_a_catalog_whose_stats_reference_a_missing_table`と同じ
        // 手順で、カタログのバイト列を直接組み立てて再現する。
        let path = temp_path("stats-open-distinct-zero");
        Storage::create(&path).unwrap();

        let mut tables = HashMap::new();
        let table_id = TableId(0);
        tables.insert(
            table_id,
            TableEntry { info: TableInfo { id: table_id, name: "t".to_string(), schema: one_bigint_schema() }, page_ids: Vec::new() },
        );

        let mut column = valid_column_stats();
        column.distinct_count = 0; // 非NULL行(10行)があるのに0
        let mut stats = HashMap::new();
        stats.insert(table_id, TableStats { row_count: 100, columns: vec![column] });

        let encoded = encode_catalog(1, &tables, &[], &[], &stats);
        let mut payload = vec![0u8; PAGE_PAYLOAD_SIZE];
        payload[..encoded.len()].copy_from_slice(&encoded);

        let mut page = Page::new(CATALOG_PAGE_ID, PageType::Catalog);
        page.payload_mut().copy_from_slice(&payload);
        let bytes = page.encode();

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        file.write_all(&bytes).unwrap();

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_min_greater_than_max() {
        let path = temp_path("stats-min-max");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.min = Some(Value::BigInt(9));
        column.max = Some(Value::BigInt(0));
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_a_column_type_mismatch_in_min_max() {
        let path = temp_path("stats-type-mismatch");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.min = Some(Value::Text("0".to_string())); // 列の型はBIGINT
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_a_bucket_count_above_the_limit() {
        let path = temp_path("stats-bucket-limit");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.histogram.push(Bucket { lower: Value::BigInt(9), upper: Value::BigInt(9), row_count: 0 });
        // 11バケツはHISTOGRAM_BUCKET_COUNT(10)を超える。
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_histogram_buckets_out_of_order() {
        let path = temp_path("stats-bucket-order");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.histogram.swap(0, 1); // バケツの並びが昇順でなくなる
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    /// codexレビュー4巡目の再現: 値`1`を1行ずつ持つ、同一境界(`lower == upper
    /// == 1`)の2バケツ。`crate::estimator::equality_selectivity_within_non_null`
    /// は「同じ値を含むHistogramバケツは必ず1個」という不変条件に依存して
    /// おり(`crate::statistics`モジュールの説明を参照)、この2バケツを
    /// 許すと`v = 1`の選択率が最初の1バケツぶんの1行だけを見て見積もられ、
    /// 実際の2行の半分(期待値0.10に対して0.05)になる。
    fn column_stats_with_a_duplicate_bucket_boundary() -> ColumnStats {
        ColumnStats {
            null_count: 0,
            distinct_count: 1,
            min: Some(Value::BigInt(1)),
            max: Some(Value::BigInt(1)),
            mcv: Vec::new(),
            histogram: vec![
                Bucket { lower: Value::BigInt(1), upper: Value::BigInt(1), row_count: 1 },
                Bucket { lower: Value::BigInt(1), upper: Value::BigInt(1), row_count: 1 },
            ],
        }
    }

    #[test]
    fn set_table_stats_rejects_adjacent_buckets_that_share_the_same_boundary_value() {
        let path = temp_path("stats-bucket-duplicate-boundary");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let stats = TableStats { row_count: 2, columns: vec![column_stats_with_a_duplicate_bucket_boundary()] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_catalog_whose_adjacent_buckets_share_the_same_boundary_value() {
        // set_table_statsは常にvalidate_one_table_statsを通すため、この不整合は
        // `open_rejects_a_catalog_whose_stats_reference_a_missing_table`と同じ
        // 手順で、カタログのバイト列を直接組み立てて再現する。この形式
        // (バケツ境界がバケツ間で重複しうる統計)は、この教材のブランチの
        // 途中コミット(第4部3巡目レビュー対応より前)でのみ生成されえた
        // ものであり、リリース済みの章のファイル形式ではない。章をまたいだ
        // ファイル互換性を約束しない方針(モジュール冒頭を参照)の範囲内で、
        // この検証強化により再オープン時に決定的に`CorruptCatalog`として
        // 拒否されることを確認する(第4部4巡目レビュー対応)。
        let path = temp_path("stats-open-bucket-duplicate-boundary");
        Storage::create(&path).unwrap();

        let mut tables = HashMap::new();
        let table_id = TableId(0);
        tables.insert(
            table_id,
            TableEntry { info: TableInfo { id: table_id, name: "t".to_string(), schema: one_bigint_schema() }, page_ids: Vec::new() },
        );

        let mut stats = HashMap::new();
        stats.insert(table_id, TableStats { row_count: 2, columns: vec![column_stats_with_a_duplicate_bucket_boundary()] });

        let encoded = encode_catalog(1, &tables, &[], &[], &stats);
        let mut payload = vec![0u8; PAGE_PAYLOAD_SIZE];
        payload[..encoded.len()].copy_from_slice(&encoded);

        let mut page = Page::new(CATALOG_PAGE_ID, PageType::Catalog);
        page.payload_mut().copy_from_slice(&payload);
        let bytes = page.encode();

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        file.write_all(&bytes).unwrap();

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_a_bucket_row_count_sum_mismatch() {
        let path = temp_path("stats-bucket-sum");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.histogram[0].row_count = 2; // 合計が非NULL行数(10)と合わなくなる
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_too_many_mcv_entries() {
        let path = temp_path("stats-mcv-limit");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.mcv = (0..=MCV_MAX_ENTRIES as i64).map(|v| (Value::BigInt(v), 1)).collect(); // 11件
        column.histogram = Vec::new();
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn set_table_stats_rejects_a_duplicate_mcv_value() {
        let path = temp_path("stats-mcv-duplicate");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("t", one_bigint_schema()).unwrap();
        let mut column = valid_column_stats();
        column.mcv = vec![(Value::BigInt(0), 5), (Value::BigInt(0), 3)];
        column.histogram = vec![Bucket { lower: Value::BigInt(1), upper: Value::BigInt(9), row_count: 2 }];
        let stats = TableStats { row_count: 100, columns: vec![column] };
        let err = expect_err(storage.set_table_stats(table_id, stats));
        assert!(matches!(err, DbError::InvalidStats(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn open_rejects_a_catalog_whose_stats_reference_a_missing_table() {
        // set_table_statsは常にvalidate_one_table_statsを通すため、この不整合は
        // Storage::openの経路(encode_catalog/decode_catalogを直接使う)でしか
        // 再現できない。テーブルを作らずに、統計だけを持つカタログのバイト列を
        // 直接組み立て、`a_structurally_valid_but_nonsensical_catalog_is_rejected_as_corrupt`
        // と同じ手順でCatalogページへ書き込む。
        let path = temp_path("stats-open-missing-table");
        Storage::create(&path).unwrap();

        let mut stats = HashMap::new();
        stats.insert(TableId(0), valid_table_stats());
        let encoded = encode_catalog(1, &HashMap::new(), &[], &[], &stats);
        let mut payload = vec![0u8; PAGE_PAYLOAD_SIZE];
        payload[..encoded.len()].copy_from_slice(&encoded);

        let mut page = Page::new(CATALOG_PAGE_ID, PageType::Catalog);
        page.payload_mut().copy_from_slice(&payload);
        let bytes = page.encode();

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(PAGE_SIZE as u64)).unwrap();
        file.write_all(&bytes).unwrap();

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)), "err={err:?}");
        std::fs::remove_file(&path).unwrap();
    }

    /// [`CATALOG_MAGIC`]・[`CATALOG_LAYOUT_VERSION`]・`is_constraint`のいずれも
    /// 持たない、この章より前のカタログレイアウトを手書きで再現する
    /// (第3部レビュー対応の回帰テスト)。
    ///
    /// 索引メタデータのレイアウトだけが、`unique: u8`・`primary_key: u8`・
    /// `key_type: u8`で終わる(`is_constraint`が無い)点で現行の`encode_catalog`と
    /// 異なる。それ以外(`next_table_id`・テーブル定義・Free Page List)は
    /// このモジュール冒頭のドキュメントに記録されている、この時点までの
    /// レイアウトのままである。
    fn encode_pre_magic_catalog_without_is_constraint(
        next_table_id: u64,
        tables: &HashMap<TableId, TableEntry>,
        indexes: &[(&str, TableId, usize, &str, bool, bool, DataType)],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&next_table_id.to_le_bytes());
        out.extend_from_slice(&(tables.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // free_page_count

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

        out.extend_from_slice(&(indexes.len() as u32).to_le_bytes());
        for &(name, table_id, column_index, column_name, unique, primary_key, key_type) in indexes {
            let name_bytes = name.as_bytes();
            out.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(name_bytes);
            out.extend_from_slice(&table_id.0.to_le_bytes());
            out.extend_from_slice(&(column_index as u16).to_le_bytes());
            let column_name_bytes = column_name.as_bytes();
            out.extend_from_slice(&(column_name_bytes.len() as u16).to_le_bytes());
            out.extend_from_slice(column_name_bytes);
            out.push(u8::from(unique));
            out.push(u8::from(primary_key));
            out.push(data_type_to_u8(key_type));
            // is_constraintバイトは無い(このレイアウトにはまだ存在しない)。
        }
        out
    }

    /// レビュー指摘の再現条件その1: 制約索引を1本だけ持つ、`is_constraint`
    /// フィールド導入前のカタログ。
    ///
    /// `CATALOG_MAGIC`・`CATALOG_LAYOUT_VERSION`を持たないこの旧レイアウトを
    /// 現在のコードでそのまま`open`すると、Catalogページのpayloadは
    /// (`persist_catalog`が末尾を0で埋めるため)実データの直後から0が
    /// 続いている。`is_constraint`を検査する前の実装は、この0を
    /// `is_constraint = false`として黙って受理してしまい、`DROP INDEX`で
    /// 制約索引を削除できてしまう不整合につながっていた
    /// (`crate::index`の回帰テストが、その不整合自体は別に再現している)。
    /// マジックバイト列による版検査を追加した現在は、`open`の時点で
    /// 決定的に`DbError::CorruptCatalog`を返し、レイアウトを読み違えたまま
    /// 受理することがない。
    #[test]
    fn open_rejects_a_pre_is_constraint_catalog_with_a_single_constraint_index() {
        let path = temp_path("legacy-catalog-single-constraint-index");
        Storage::create(&path).unwrap();

        let schema = Schema::new(vec![Column::new("id", DataType::BigInt, false).with_primary_key(), Column::new("name", DataType::Text, true)]);
        let table_id = TableId(0);
        let mut tables = single_table_entry(table_id, "users", Vec::new());
        tables.get_mut(&table_id).unwrap().info.schema = schema;

        let indexes = [("users_id_idx", table_id, 0usize, "id", true, true, DataType::BigInt)];
        let bytes = encode_pre_magic_catalog_without_is_constraint(1, &tables, &indexes);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)), "旧レイアウトは決定的に拒否されるはず: {err:?}");

        std::fs::remove_file(&path).unwrap();
    }

    /// レビュー指摘の再現条件その2: 制約索引を複数本(`PRIMARY KEY`・`UNIQUE`)
    /// 持つ、`is_constraint`フィールド導入前のカタログ。
    ///
    /// 索引が複数本ある場合、`is_constraint`検査が無いだけでは済まない。
    /// 1本目の索引レコードが`is_constraint`ぶんの1バイトだけ短いため、
    /// 2本目以降の索引レコードは読み出し位置が1バイトずつ左へずれ、
    /// 2本目の`name_len`の上位バイトを前の索引の`key_type`として読むなど、
    /// フィールドの境界そのものが崩れる。マジックバイト列による版検査は、
    /// この種のずれを個別に検査するのではなく、レイアウト全体を
    /// 決定的に拒否することで防ぐ。
    #[test]
    fn open_rejects_a_pre_is_constraint_catalog_with_multiple_constraint_indexes() {
        let path = temp_path("legacy-catalog-multiple-constraint-indexes");
        Storage::create(&path).unwrap();

        let schema = Schema::new(vec![
            Column::new("id", DataType::BigInt, false).with_primary_key(),
            Column::new("email", DataType::Text, true).with_unique(),
        ]);
        let table_id = TableId(0);
        let mut tables = single_table_entry(table_id, "users", Vec::new());
        tables.get_mut(&table_id).unwrap().info.schema = schema;

        let indexes = [
            ("users_id_idx", table_id, 0usize, "id", true, true, DataType::BigInt),
            ("users_email_idx", table_id, 1usize, "email", true, false, DataType::Text),
        ];
        let bytes = encode_pre_magic_catalog_without_is_constraint(1, &tables, &indexes);
        write_catalog_payload(&path, &bytes);

        let err = expect_err(Storage::open(&path));
        assert!(matches!(err, DbError::CorruptCatalog(_)), "旧レイアウトは決定的に拒否されるはず: {err:?}");

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
        let bytes = encode_catalog(1, &tables, &[PageId(0)], &[], &HashMap::new());
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
        let bytes = encode_catalog(1, &tables, &[], &[], &HashMap::new());
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
            let bytes = encode_catalog(1, &tables, &[], &[], &HashMap::new());
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
        let bytes = encode_catalog(2, &tables, &[], &[], &HashMap::new());
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
        let bytes = encode_catalog(0, &tables, &[], &[], &HashMap::new());
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
        let bytes = encode_catalog(u64::MAX, &tables, &[], &[], &HashMap::new());
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
        let bytes = encode_catalog(2, &tables, &[], &[], &HashMap::new());
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

    // ---- 第39章: 索引利用回数・Buffer Pool統計 ----

    #[test]
    fn index_usage_counts_starts_at_zero_and_increases_with_record_index_use() {
        let (path, idx_path) = index_test_paths("index-usage");
        let mut storage = Storage::create(&path).unwrap();
        storage.create_table("users", users_schema()).unwrap();
        storage.create_index("idx", "users", "name", false).unwrap();

        assert_eq!(storage.index_usage_counts(), vec![("idx".to_string(), 0)]);

        storage.record_index_use("idx");
        storage.record_index_use("idx");
        assert_eq!(storage.index_usage_counts(), vec![("idx".to_string(), 2)]);

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&idx_path);
    }

    #[test]
    fn drop_index_forgets_its_usage_count() {
        let (path, idx_path) = index_test_paths("index-usage-drop");
        let mut storage = Storage::create(&path).unwrap();
        storage.create_table("users", users_schema()).unwrap();
        storage.create_index("idx", "users", "name", false).unwrap();
        storage.record_index_use("idx");

        storage.drop_index("idx").unwrap();
        assert!(storage.index_usage_counts().is_empty());

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&idx_path);
    }

    #[test]
    fn buffer_pool_stats_reflects_hits_and_misses() {
        let path = temp_path("buffer-pool-stats");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();
        insert_user_row(&mut storage, table_id, 1, "alice");

        let before = storage.buffer_pool_stats();
        let _ = storage.scan(table_id).unwrap().count();
        let after = storage.buffer_pool_stats();
        assert!(after.hits + after.misses > before.hits + before.misses);

        std::fs::remove_file(&path).unwrap();
    }

    // ---- 第39章: VACUUM ----

    #[test]
    fn vacuum_table_reclaims_fully_emptied_pages_into_the_free_page_list() {
        let path = temp_path("vacuum-reclaim");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();

        let long_name = "x".repeat(300);
        let mut rids = Vec::new();
        for i in 0..80i64 {
            rids.push(insert_user_row(&mut storage, table_id, i, &long_name));
        }
        let pages_before = storage.table_page_count(table_id).unwrap();
        assert!(pages_before > 1, "複数ページにまたがっているはず: {pages_before}");

        for &rid in &rids {
            assert!(storage.delete(table_id, rid).unwrap());
        }
        assert_eq!(
            storage.table_page_count(table_id).unwrap(),
            pages_before,
            "DELETEだけ(compactを挟まない)ではページ数は減らない(Tombstoneのまま)"
        );

        let report = storage.vacuum_table(table_id).unwrap();
        assert_eq!(report.reclaimed_pages, pages_before as usize, "全行を削除したので全ページが回収されるはず");
        assert_eq!(storage.table_page_count(table_id).unwrap(), 0);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn vacuum_table_does_not_reclaim_a_page_that_still_holds_a_live_row() {
        let path = temp_path("vacuum-partial");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();

        let r1 = insert_user_row(&mut storage, table_id, 1, "alice");
        let _r2 = insert_user_row(&mut storage, table_id, 2, "bob");
        assert!(storage.delete(table_id, r1).unwrap());

        let pages_before = storage.table_page_count(table_id).unwrap();
        let report = storage.vacuum_table(table_id).unwrap();
        assert_eq!(report.reclaimed_pages, 0, "bobがまだ生きているのでページは回収されない");
        assert_eq!(storage.table_page_count(table_id).unwrap(), pages_before);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn vacuum_table_reports_the_number_of_rebuilt_indexes() {
        let (path, idx_path) = index_test_paths("vacuum-rebuilt-count");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();
        insert_user_row(&mut storage, table_id, 1, "alice");
        storage.create_index("idx", "users", "name", false).unwrap();

        let report = storage.vacuum_table(table_id).unwrap();
        assert_eq!(report.rebuilt_indexes, 1);
        // 索引は`rename`によって同じ名前のまま引き続き使える。
        assert!(storage.index("idx").is_some());
        assert_eq!(storage.index_btree("idx").unwrap().lookup(&Value::Text("alice".to_string())).unwrap().len(), 1);

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&idx_path);
    }

    // ---- 第3部2巡目レビュー対応: create_table_with_constraint_indexesの原子性 ----

    #[test]
    fn create_table_with_constraint_indexes_makes_a_table_and_all_its_constraint_indexes_together() {
        let path = temp_path("create-table-with-constraints-ok");
        let mut storage = Storage::create(&path).unwrap();
        let schema = Schema::new(vec![
            Column::new("id", DataType::BigInt, false).with_primary_key(),
            Column::new("email", DataType::Text, true).with_unique(),
            Column::new("name", DataType::Text, true),
        ]);
        let constraint_columns = vec![("id".to_string(), true), ("email".to_string(), false)];
        let table_id = storage.create_table_with_constraint_indexes("users", schema, &constraint_columns).unwrap();

        assert_eq!(storage.table("users").unwrap().id, table_id);
        let id_idx = storage.index("users_id_idx").unwrap();
        assert!(id_idx.unique && id_idx.primary_key);
        let email_idx = storage.index("users_email_idx").unwrap();
        assert!(email_idx.unique && !email_idx.primary_key);

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(index_file_path(&path, "users_id_idx"));
        let _ = std::fs::remove_file(index_file_path(&path, "users_email_idx"));
    }

    /// レビュー指摘の再現条件: 索引ファイルのパス(`<DBファイル名>.idx.<索引名>`)が
    /// OSのファイル名長の上限を超えるほど長いテーブル名を`PRIMARY KEY`付きで
    /// 指定すると、索引ファイルの作成そのものがI/Oエラーで失敗する。この
    /// 失敗は索引名の衝突ではないため、事前の名前衝突検査では防げない。
    /// `create_table_with_constraint_indexes`は、テーブルの登録も含めて
    /// 一度もカタログへ永続化していない時点でこの失敗を検出するため、
    /// テーブル自体もカタログに一切残らないはずである。
    #[test]
    fn create_table_with_constraint_indexes_leaves_no_table_when_the_index_file_path_is_too_long() {
        let path = temp_path("create-table-with-constraints-name-too-long");
        let mut storage = Storage::create(&path).unwrap();
        let long_table_name = "t".repeat(245);
        let schema = Schema::new(vec![Column::new("id", DataType::BigInt, false).with_primary_key()]);
        let constraint_columns = vec![("id".to_string(), true)];

        let err = expect_err(storage.create_table_with_constraint_indexes(&long_table_name, schema, &constraint_columns));
        assert!(matches!(err, DbError::Io(_)), "索引ファイルのパス長超過はI/Oエラーとして観測されるはず: {err:?}");

        // テーブル自体もカタログに残っていないはず。
        assert!(storage.table(&long_table_name).is_none());
        let index_name = format!("{long_table_name}_id_idx");
        assert!(storage.index(&index_name).is_none());

        // 別の(短い)名前なら、直後でも問題なくテーブルを作れる
        // (next_table_id・カタログのどちらも壊れていないことの確認)。
        let ok_id = storage
            .create_table_with_constraint_indexes("users", users_schema(), &[])
            .unwrap();
        assert_eq!(storage.table("users").unwrap().id, ok_id);

        std::fs::remove_file(&path).unwrap();
    }

    /// 2本目以降の制約索引が失敗したとき、それより前にすでに`self.indexes`へ
    /// 登録済み・ファイルも作成済みだった1本目の索引まで、テーブルと一緒に
    /// 巻き戻されることを確認する(索引名の衝突以外でも失敗しうる、という
    /// 一般的な保険が正しく効くかどうかは、1本目が「すでに出来上がった後」で
    /// 2本目が失敗するこの順序でしか確認できない)。
    ///
    /// 2本目の失敗理由には、無関係な既存テーブル`dummy`が先取りしている
    /// 索引名との衝突を使う。`dummy`側の索引ファイルは、`users`の作成とは
    /// 無関係な既存の資産なので、巻き戻しの対象にならず残り続けるはずである
    /// (巻き戻しが「衝突した名前のファイルを消す」という誤った実装になって
    /// いないことも合わせて確認する)。
    #[test]
    fn create_table_with_constraint_indexes_rolls_back_the_table_and_earlier_indexes_when_a_later_index_fails() {
        let path = temp_path("create-table-with-constraints-second-index-fails");
        let idx_id_path = index_file_path(&path, "users_id_idx");
        let dummy_idx_path = index_file_path(&path, "users_email_idx");
        let mut storage = Storage::create(&path).unwrap();

        // "users_email_idx"という名前を、無関係な既存テーブル"dummy"の索引と
        // して先取りしておく。
        storage.create_table("dummy", users_schema()).unwrap();
        storage.create_index("users_email_idx", "dummy", "name", false).unwrap();
        assert!(dummy_idx_path.exists());

        let schema = Schema::new(vec![
            Column::new("id", DataType::BigInt, false).with_primary_key(),
            Column::new("email", DataType::Text, true).with_unique(),
        ]);
        let constraint_columns = vec![("id".to_string(), true), ("email".to_string(), false)];
        let err = expect_err(storage.create_table_with_constraint_indexes("users", schema, &constraint_columns));
        assert!(matches!(err, DbError::DuplicateIndex(name) if name == "users_email_idx"));

        assert!(storage.table("users").is_none(), "テーブルも残っていないはず");
        assert!(storage.index("users_id_idx").is_none(), "先に出来上がっていた1本目の索引も巻き戻されているはず");
        assert!(!idx_id_path.exists(), "1本目の索引ファイルも削除されているはず");

        // "dummy"の索引はusersの作成とは無関係なので、そのまま残っている。
        assert!(storage.index("users_email_idx").unwrap().table_id == storage.table("dummy").unwrap().id);
        assert!(dummy_idx_path.exists(), "無関係な既存索引のファイルは巻き戻しの対象にならないはず");

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(dummy_idx_path);
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

    /// 第3部2巡目レビュー対応の回帰テスト: `drop_index`(SQLの`DROP INDEX`が
    /// 呼ぶ入口)は`IndexInfo::is_constraint`な索引を拒否する。`PRIMARY KEY`・
    /// `UNIQUE`のどちらの制約索引についても確認したうえで、`drop_table`
    /// (内部経路、`drop_index_impl`を使う)なら同じ索引を問題なく削除できる
    /// ことも確認する。
    #[test]
    fn drop_index_rejects_a_constraint_index_but_drop_table_can_still_remove_it() {
        let path = temp_path("drop-index-rejects-constraint");
        let idx_id_path = index_file_path(&path, "users_id_idx");
        let idx_email_path = index_file_path(&path, "users_email_idx");
        let mut storage = Storage::create(&path).unwrap();
        let schema = Schema::new(vec![
            Column::new("id", DataType::BigInt, false).with_primary_key(),
            Column::new("email", DataType::Text, true).with_unique(),
        ]);
        storage
            .create_table_with_constraint_indexes("users", schema, &[("id".to_string(), true), ("email".to_string(), false)])
            .unwrap();
        assert!(storage.index("users_id_idx").unwrap().is_constraint);
        assert!(storage.index("users_email_idx").unwrap().is_constraint);

        let err = expect_err(storage.drop_index("users_id_idx"));
        assert!(matches!(err, DbError::CannotDropConstraintIndex(name) if name == "users_id_idx"));
        let err = expect_err(storage.drop_index("users_email_idx"));
        assert!(matches!(err, DbError::CannotDropConstraintIndex(name) if name == "users_email_idx"));
        // 拒否されただけで、索引自体はそのまま残っている。
        assert!(storage.index("users_id_idx").is_some());
        assert!(idx_id_path.exists());

        storage.drop_table("users").unwrap();
        assert!(storage.index("users_id_idx").is_none());
        assert!(storage.index("users_email_idx").is_none());
        assert!(!idx_id_path.exists());
        assert!(!idx_email_path.exists());

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

    /// 第3部レビュー対応の回帰テスト: `table_id`が複数の索引を持つ状態で、
    /// そのうち1つの索引だけが`DbError::BTreeKeyTooLarge`で拒否する行を
    /// `index_insert_row`へ渡すと、それより前に成功していた(他の)索引への
    /// 反映が巻き戻され、この`rid`がどの索引にも残らないことを確認する。
    /// `self.indexes`は`HashMap`で走査順が非決定的なため、"name"索引が先に
    /// 成功してから"id"索引で失敗する場合と、その逆の場合のどちらが起きても
    /// この不変条件は保たれるべきである。
    #[test]
    fn index_insert_row_rolls_back_earlier_indexes_when_a_later_index_rejects_the_key() {
        let path = temp_path("index-insert-row-rollback");
        let flag_idx_path = index_file_path(&path, "flag_idx");
        let name_idx_path = index_file_path(&path, "name_idx");

        // `id: BIGINT`の代わりに1バイトで符号化される`flag: BOOLEAN`を使い、
        // 「Heapの1行としては収まるが、`name`列を索引化したB+Treeの空の
        // Leaf Page1枚には収まらない」という値の幅を作る(`users_schema`の
        // `id: BIGINT`(8バイト)ではこの幅が存在しない。Heap側のタプル
        // エンコーディングの固定オーバーヘッドがB+Tree側の固定オーバーヘッド
        // より小さいため、Heapに収まる値は常にB+Treeにも収まってしまう)。
        let schema = Schema::new(vec![Column::new("flag", DataType::Boolean, false), Column::new("name", DataType::Text, true)]);

        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", schema.clone()).unwrap();
        storage.create_index("flag_idx", "users", "flag", false).unwrap();
        storage.create_index("name_idx", "users", "name", false).unwrap();

        // Heapのタプル1件が収まる上限は`PAGE_PAYLOAD_SIZE - 12`バイト、
        // このタプルの`name`以外の部分(bitmap 1 + flag 1 + 長さ接頭辞 4)は
        // 6バイトなので、`name`は最大`PAGE_PAYLOAD_SIZE - 18`バイトまで
        // Heapに収まる。一方name_idx(B+Tree)の空のLeaf Page1枚に収まる
        // キーの上限は`PAGE_PAYLOAD_SIZE - 24`バイト。この2つの間の長さの
        // `name`を選べば、Heapには収まるがname_idxには収まらない。
        let huge_name = "x".repeat(crate::page::PAGE_PAYLOAD_SIZE - 20);
        let tuple = Tuple::new(&schema, vec![Value::Boolean(true), Value::Text(huge_name)]).unwrap();
        let bytes = crate::tuple_codec::encode_tuple(&schema, &tuple);
        let rid = storage.insert(table_id, &bytes).unwrap();

        let err = expect_err(storage.index_insert_row(table_id, &tuple, rid));
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        // flag_idxが先に成功していたとしても、巻き戻されてこの`rid`を指す
        // エントリは残っていないはず。
        let flag_entries: Vec<_> = storage
            .index_btree("flag_idx")
            .unwrap()
            .range(std::ops::Bound::Unbounded, std::ops::Bound::Unbounded)
            .unwrap()
            .collect::<DbResult<Vec<_>>>()
            .unwrap();
        assert!(flag_entries.iter().all(|(_, r)| *r != rid), "flag_idxにこのrid宛のエントリが残っている: {flag_entries:?}");

        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_file(&flag_idx_path);
        let _ = std::fs::remove_file(&name_idx_path);
    }

    /// `check_indexes_accept_row`が、対象となる索引のうち1つでもキーが
    /// 収まらなければ、Heapへの書き込みより前に(何にも触れずに)エラーを
    /// 返すことを確認する(第3部レビュー対応の主防御)。
    #[test]
    fn check_indexes_accept_row_rejects_before_touching_anything() {
        let (path, idx_path) = index_test_paths("check-indexes-accept-row");
        let mut storage = Storage::create(&path).unwrap();
        let table_id = storage.create_table("users", users_schema()).unwrap();
        storage.create_index("idx", "users", "name", false).unwrap();

        let schema = users_schema();
        let huge_name = "x".repeat(crate::page::PAGE_PAYLOAD_SIZE);
        let tuple = Tuple::new(&schema, vec![Value::BigInt(1), Value::Text(huge_name)]).unwrap();

        let err = expect_err(storage.check_indexes_accept_row(table_id, &tuple));
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        // 小さい値なら通る。
        let ok_tuple = Tuple::new(&schema, vec![Value::BigInt(1), Value::Text("alice".to_string())]).unwrap();
        storage.check_indexes_accept_row(table_id, &ok_tuple).unwrap();

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
