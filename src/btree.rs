//! キーから`RecordId`を引くSecondary Indexとしての、ディスク上のB+Tree。
//!
//! `Storage`(第15章)の`get`は`RecordId`を知っている前提でタプルを1件返すが、
//! `RecordId`そのものを求める手段は、第20章の時点まで「テーブル全体を先頭から
//! 走査し、値が一致する行を探す」という線形探索(`O(n)`)しかなかった。この章の
//! `BTree`は、あるキーに対応する`RecordId`の集まりを`O(log n)`(木の高さ)の
//! ページ読み込みで求められる索引を提供する。
//!
//! `BTree`が保持するのはキーと`RecordId`の対応だけであり、行の実体
//! (タプルのバイト列)は持たない。行そのものは引き続き`Storage`(あるいは
//! `HeapFile`)が保持し、`BTree::lookup`が返した`RecordId`を使って読みに行く。
//! この形の索引をSecondary Indexと呼ぶ。
//!
//! # スコープ: この章で実装するもの、しないもの
//!
//! 実装するのは、`BTree::create`・`BTree::open`という入口と、Point Lookup
//! (`lookup`)、Insert(`insert`)、そしてInsertが引き起こすLeaf Split・
//! Internal Split・Root Splitである。
//!
//! 次の機能はこの章の範囲外とし、第24章(B+Tree II)に譲る。
//!
//! - **Range Scan**: `key >= x AND key <= y`のような範囲検索。この章のLeaf
//!   Pageはまだ隣接する葉への横方向のリンクを持たない。
//! - **Delete**: この章はRoot Splitまで(木が成長する方向)しか実装しない。
//!   縮小方向の操作(Redistribution、Merge、Root縮小)は行わない。
//! - **`CREATE INDEX`とIndex Maintenance**: 既存の行から索引を作る、
//!   `INSERT`・`UPDATE`・`DELETE`のたびに索引を追従させる、といった
//!   `Storage`・`Database`との結線はまだ無い。この章の`BTree`は、
//!   `Storage`と同じ`BufferPool`の上で動く独立したデータ構造にとどまる。
//!
//! # キーのエンコーディング
//!
//! このSQLサブセットが対応する索引キーは単一列に限る(複合キーは扱わない)。
//! キーの型は`DataType`(`BOOLEAN`・`BIGINT`・`TEXT`のいずれか)で、`BTree::create`の
//! 時点で1つに固定する。
//!
//! キーは、Leaf Page・Internal Pageの二分探索([`crate::btree_page`])が
//! **バイト列としての大小比較だけ**で正しく動くよう、**順序を保存するバイト列**へ
//! エンコードする(`encode_key`)。この設計を選んだ理由は、比較のたびにバイト列を
//! `Value`へ復元する必要がなくなることにある。復元(`decode_key`)は、この章では
//! デバッグや将来のRange Scan(第24章)のために用意してあるだけで、`lookup`・
//! `insert`の探索経路そのものは一度も`decode_key`を呼ばない。
//!
//! | `DataType` | エンコード方式 |
//! | --- | --- |
//! | `BOOLEAN` | `0`(false)または`1`(true)の1バイト。 |
//! | `BIGINT` | `i64`の符号ビットを反転してから`u64`としてビッグエンディアンで並べた8バイト。符号ビットを反転するのは、2の補数表現の`i64`をそのままビッグエンディアンにしても、負の数(先頭ビットが1)が正の数より大きいバイト列になってしまい、バイト列としての大小関係が数値としての大小関係と逆転する範囲が生まれるため。符号ビットを反転させると、バイト列の大小関係が`i64`全域で数値の大小関係と一致する。 |
//! | `TEXT` | UTF-8バイト列をそのまま使う。Rustの`&[u8]`(`Vec<u8>`)の`Ord`はバイト列の辞書式順序であり、これは`"ab" < "abc"`のように短い文字列を長い文字列の接頭辞として正しく先に並べる。UTF-8のバイト表現は、Unicodeの基本多言語面の範囲では符号点順とバイト列としての辞書式順序が一致するため、追加のエンコードなしにそのまま索引キーとして使える。 |
//!
//! この章のSQLサブセットが持つ型は3つしかないため、`BOOLEAN`と`BIGINT`のように
//! バイト数が固定の型でも、キーの前後に型タグを持たせていない。`BTree`自身が
//! `key_type: DataType`をMetaページ(後述)へ永続化し、`insert`・`lookup`の
//! 呼び出しのたびにその型と食い違う`Value`を渡していないか検査する
//! (`check_key_type`)。これにより、あるキー1本のバイト列だけを見て型を
//! 判定する必要が無くなり、エンコードも1バイトから始められる。
//!
//! # `NULL`はキーにしない
//!
//! `Value::Null`を`insert`・`lookup`に渡すと`DbError::NullKeyNotAllowed`を返す。
//! SQLの`UNIQUE`・`PRIMARY KEY`が「`NULL`同士は重複とみなさない」という規則
//! (第20章の`crate::constraints`)を持つのと同じ理由で、`NULL`は「値が
//! 分からない」ことを表すのであって、索引のキー空間上のどこかの位置を指す
//! 値ではない。`col IS NULL`という検索は、この索引では答えられない
//! (`NULL`を持つ行を除外した索引から`col IS NULL`を引こうとしても、
//! そもそも該当する行が索引に登録されていない)。`NULL`を持つ行をそもそも
//! 索引へ挿入しない、という判断は`BTree`自身の役割ではなく、`INSERT`・
//! `UPDATE`のたびに`BTree::insert`を呼ぶかどうかを決める呼び出し側
//! (第24章のIndex Maintenance)の責務である。この章の`BTree`は、
//! 呼び出し側がうっかり`NULL`を渡した場合の防御としてこのエラーを返す。
//!
//! # 重複キーの扱い
//!
//! `insert`は、同じキーを持つエントリを何度でも受け付ける。`PRIMARY KEY`・
//! `UNIQUE`の一意性検査(第20章の`crate::constraints`と同じ役割のもの)を
//! この索引自身が行うようにする変更は、第24章でIndex Maintenanceを実装する
//! ときに扱う。この章の`BTree`はキーの一意性に一切関知しない、ただの
//! 「キー→`RecordId`」の多重写像である。
//!
//! `lookup`は、一致したキーが1ページ(Leaf Page)の中に収まっている限り、
//! そのキーを持つ全エントリを返す。ただし、同じキーを持つエントリが多すぎて
//! Leaf Splitによって複数のLeaf Pageへ分かれてしまった場合、この章の`lookup`は
//! 分割後にたどり着いた1ページの中の一致だけを返し、隣接するページにはみ出した
//! 分を取りこぼす。この章のLeaf Pageはまだ横方向のリンクを持たないため、
//! 「隣のページも見に行く」という動作を実装できない。第24章でLeaf間リンクと
//! Range Scanが揃えば、この取りこぼしは解消される。
//!
//! # ページへのアクセスと並行性
//!
//! ページの読み書きはすべて`BufferPool`(第14章)の`PageReadGuard`・
//! `PageWriteGuard`経由で行う。`insert`は、あるページを書き込み用にpinしている
//! 間は同じページを二重に`pin`しない(`BufferPool`の`Mutex`は再入可能ではない
//! ため、二重にpinしようとするとデッドロックする)。分割の伝播([`BTree::insert`]の
//! 後半)は、親から子へ1ページずつ順番にpinと解放を繰り返す設計にしており、
//! 複数階層のページを同時にpinし続けることはない。
//!
//! `minidb`はこの章の時点でまだシングルスレッドで動作しており、複数のスレッドが
//! 同時にこの`BTree`へ`insert`する状況は扱わない。ページ単位のLatch(読み書き
//! ロック)によって複数スレッドから安全にB+Treeを操作できるようにするのは
//! 第35章の仕事である。
//!
//! # Metaページ
//!
//! `BTree`はページ1(ページ0はDiskManagerのFile Headerが占有する)を、
//! 現在のRootページと、このツリーのキー型を保持するMetaページとして使う。
//! `Storage`(第15章)がCatalogページ専用の`PageType::Catalog`を新設したのとは
//! 対照的に、この章では新しいPage Typeを追加せず、既存の`PageType::Data`を
//! 転用する。`Storage`のCatalogページは複数のテーブル定義という可変長の
//! コレクションを保持する必要があったが、この章のMetaページが持つ情報は
//! 「Rootの`PageId`(8バイト)」と「キー型(1バイト)」という固定長の2値だけで
//! あり、`Data`ページと区別する構造的な理由が無い。
//!
//! ```text
//! offset 0        8         9
//! +----------------+---------+
//! | root_page_id   | key_type|
//! | (8バイト、LE)   | (1バイト)|
//! +----------------+---------+
//! ```
//!
//! `key_type`は`0`(`BOOLEAN`)・`1`(`BIGINT`)・`2`(`TEXT`)のいずれかで、
//! `Storage`のCatalogページが列の型を符号化するのに使ったコード(モジュール
//! `crate::storage`のドキュメント参照)と同じ割り当てにしてある。

use crate::buffer_pool::BufferPool;
use crate::error::{DbError, DbResult};
use crate::ids::{PageId, RecordId};
use crate::page::PageType;
use crate::types::{DataType, Value};

use crate::btree_page::{InternalPage, InternalPageRef, LeafPage, LeafPageRef};

/// Metaページ(Rootの`PageId`とキー型)の定位置。ページ0はDiskManagerのFile
/// Headerが占有しているため、空いている最初の番号を使う(`crate::storage`の
/// `CATALOG_PAGE_ID`と同じ発想)。
const META_PAGE_ID: PageId = PageId(1);

/// キーから`RecordId`の集まりを引く、ディスク上のB+Tree。
pub struct BTree {
    pool: BufferPool,
    root: PageId,
    key_type: DataType,
}

impl BTree {
    /// `pool`が管理する新しいファイルの上に、空の`BTree`を作る。
    ///
    /// `key_type`が、以後この`BTree`が受け付けるキーの型を固定する。
    /// Metaページ(ページ1)と、空のLeaf Page1枚(初期状態のRoot)を割り当てる。
    pub fn create(pool: BufferPool, key_type: DataType) -> DbResult<Self> {
        let meta_id = pool.allocate_page(PageType::Data)?;
        debug_assert_eq!(
            meta_id, META_PAGE_ID,
            "BTreeのMetaページは、新規ファイルで2番目に確保されるページ(1番目は\
             DiskManagerが自動的に確保するFile Headerページ)である前提が崩れている"
        );

        let root_id = pool.allocate_page(PageType::BTreeLeaf)?;
        {
            let mut guard = pool.write_page(root_id)?;
            LeafPage::init(guard.data_mut());
        }

        let btree = BTree { pool, root: root_id, key_type };
        btree.write_meta()?;
        Ok(btree)
    }

    /// `pool`が管理する既存のファイルから`BTree`を復元する。
    ///
    /// Metaページ(ページ1)を読み、直近に永続化されたRootの`PageId`と
    /// キー型を復元する。
    pub fn open(pool: BufferPool) -> DbResult<Self> {
        let guard = pool.read_page(META_PAGE_ID)?;
        let data = guard.data();
        let root = PageId(u64::from_le_bytes(data[0..8].try_into().unwrap()));
        let key_type = data_type_from_u8(data[8])?;
        drop(guard);
        Ok(BTree { pool, root, key_type })
    }

    /// このツリーが受け付けるキーの型。
    pub fn key_type(&self) -> DataType {
        self.key_type
    }

    /// 現在のRootページの`PageId`(テスト・デバッグ用)。
    pub fn root_page_id(&self) -> PageId {
        self.root
    }

    /// Rootから葉までの階層数(葉だけの木は1)。
    ///
    /// 常にRootから`leftmost_child`をたどるだけで求まる。B+Treeは全ての葉が
    /// 同じ深さに揃う(`insert`がRoot Splitでのみ木を高くし、どのRootから
    /// 葉までの経路をたどっても同じ層数になる)ため、どの経路をたどっても
    /// この値は変わらない。
    pub fn height(&self) -> DbResult<usize> {
        let mut current = self.root;
        let mut height = 1;
        loop {
            let guard = self.pool.read_page(current)?;
            match guard.page_type() {
                PageType::BTreeLeaf => return Ok(height),
                PageType::BTreeInternal => {
                    let view = InternalPageRef::open(guard.data())?;
                    let next = view.leftmost_child();
                    drop(guard);
                    current = next;
                    height += 1;
                }
                other => return Err(unexpected_page_type(current, other)),
            }
        }
    }

    /// `key`に一致する全エントリの`RecordId`を返す。1件も無ければ空の`Vec`。
    ///
    /// `key`が`Value::Null`なら`DbError::NullKeyNotAllowed`、このツリーの
    /// `key_type()`と異なる型なら`DbError::BTreeKeyTypeMismatch`を返す。
    /// 同じキーを持つエントリがLeaf Splitによって複数ページへ分かれている
    /// 場合の取りこぼしについては、モジュールのドキュメント(重複キーの扱い)
    /// を参照。
    pub fn lookup(&self, key: &Value) -> DbResult<Vec<RecordId>> {
        self.check_key_type(key)?;
        let key_bytes = encode_key(key)?;

        let leaf_id = self.find_leaf(&key_bytes)?;
        let guard = self.pool.read_page(leaf_id)?;
        let view = LeafPageRef::open(guard.data())?;

        let mut result = Vec::new();
        if let Ok(i) = view.find(&key_bytes) {
            let mut lo = i;
            while lo > 0 && view.key(lo - 1) == key_bytes.as_slice() {
                lo -= 1;
            }
            let mut hi = i;
            while hi + 1 < view.entry_count() && view.key(hi + 1) == key_bytes.as_slice() {
                hi += 1;
            }
            for j in lo..=hi {
                result.push(view.record_id(j));
            }
        }
        Ok(result)
    }

    /// `key`と`rid`の対応を1件挿入する。
    ///
    /// 挿入先のLeaf Pageに空きが無ければLeaf Splitを行い、その結果を親の
    /// Internal Pageへ挿入する。親も収まらなければInternal Splitを行い、
    /// これをRootまで繰り返す。Root自体が分割された場合は、新しいInternal
    /// Pageを1枚確保してRootに据える(Root Split)。
    ///
    /// `key`が`Value::Null`なら`DbError::NullKeyNotAllowed`、このツリーの
    /// `key_type()`と異なる型なら`DbError::BTreeKeyTypeMismatch`を返す。
    /// キー1件(またはキー1本の区切りキー)だけでも空のページに収まらない
    /// ほど大きい場合は`DbError::BTreeKeyTooLarge`を返す。
    pub fn insert(&mut self, key: &Value, rid: RecordId) -> DbResult<()> {
        self.check_key_type(key)?;
        let key_bytes = encode_key(key)?;

        // Rootから葉まで下りながら、通過したInternal Pageの`PageId`を
        // `path`に記録する。分割が起きた場合、この`path`を根の方向へ
        // たどりながら親へ挿入していく(下から上への伝播)。
        let mut path: Vec<PageId> = Vec::new();
        let mut current = self.root;
        loop {
            let guard = self.pool.read_page(current)?;
            match guard.page_type() {
                PageType::BTreeLeaf => break,
                PageType::BTreeInternal => {
                    let view = InternalPageRef::open(guard.data())?;
                    let next = view.child_for(&key_bytes);
                    drop(guard);
                    path.push(current);
                    current = next;
                }
                other => return Err(unexpected_page_type(current, other)),
            }
        }
        let leaf_id = current;

        let mut pending = self.insert_into_leaf(leaf_id, &key_bytes, rid)?;

        while let Some((separator, new_page_id)) = pending {
            pending = match path.pop() {
                Some(parent_id) => self.insert_into_internal(parent_id, &separator, new_page_id)?,
                None => {
                    self.grow_new_root(&separator, new_page_id)?;
                    None
                }
            };
        }
        Ok(())
    }

    /// `leaf_id`が指すLeaf Pageへ`(key_bytes, rid)`を挿入する。収まれば
    /// `None`、Leaf Splitが起きれば`Some((区切りキー, 新しいLeaf PageのId))`
    /// を返す。
    fn insert_into_leaf(&self, leaf_id: PageId, key_bytes: &[u8], rid: RecordId) -> DbResult<Option<(Vec<u8>, PageId)>> {
        let mut entries = {
            let guard = self.pool.read_page(leaf_id)?;
            LeafPageRef::open(guard.data())?.entries()
        };
        let pos = leaf_insert_position(&entries, key_bytes);
        entries.insert(pos, (key_bytes.to_vec(), rid));

        let fits = {
            let mut guard = self.pool.write_page(leaf_id)?;
            let mut page = LeafPage::open(guard.data_mut())?;
            page.write_entries(&entries)
        };
        if fits {
            return Ok(None);
        }
        self.split_leaf(&entries, leaf_id).map(Some)
    }

    /// `parent_id`が指すInternal Pageへ、Leaf SplitまたはInternal Splitが
    /// 生んだ`(区切りキー, 新しいページのId)`を挿入する。収まれば`None`、
    /// さらにInternal Splitが起きれば`Some((区切りキー, 新しいInternal PageのId))`
    /// を返す。
    fn insert_into_internal(&self, parent_id: PageId, separator: &[u8], new_page_id: PageId) -> DbResult<Option<(Vec<u8>, PageId)>> {
        let (leftmost, mut entries) = {
            let guard = self.pool.read_page(parent_id)?;
            let view = InternalPageRef::open(guard.data())?;
            (view.leftmost_child(), view.entries())
        };
        let pos = internal_insert_position(&entries, separator);
        entries.insert(pos, (separator.to_vec(), new_page_id));

        let fits = {
            let mut guard = self.pool.write_page(parent_id)?;
            let mut page = InternalPage::open(guard.data_mut())?;
            page.write_entries(leftmost, &entries)
        };
        if fits {
            return Ok(None);
        }
        self.split_internal(&entries, leftmost, parent_id).map(Some)
    }

    /// Rootが分割された(`path`が空になった)ときに、新しいInternal Pageを
    /// 1枚確保してRootに据える(Root Split)。木の高さが1つ増える、この章の
    /// `BTree`が木を成長させる唯一の経路である。
    fn grow_new_root(&mut self, separator: &[u8], new_page_id: PageId) -> DbResult<()> {
        let new_root_id = self.pool.allocate_page(PageType::BTreeInternal)?;
        {
            let mut guard = self.pool.write_page(new_root_id)?;
            let mut page = InternalPage::init(guard.data_mut(), self.root);
            if !page.write_entries(self.root, &[(separator.to_vec(), new_page_id)]) {
                return Err(DbError::BTreeKeyTooLarge(separator.len()));
            }
        }
        self.set_root(new_root_id)
    }

    /// `entries`(挿入対象を含めた、`current_id`に収まりきらなかった全エントリ)
    /// を半分に分け、前半を`current_id`へ、後半を新しく確保したLeaf Pageへ
    /// 書き直す。戻り値は、親へ挿入すべき`(区切りキー, 新しいページのId)`。
    ///
    /// 区切りキーには後半の先頭キー(`right[0]`)をそのまま使う。このキーは
    /// 新しいLeaf Pageにも物理的にコピーされたまま残る点が、後述の
    /// [`Self::split_internal`]との非対称性である(モジュール冒頭のドキュメント
    /// 「分割の不変条件」は本文(book)で扱う)。
    fn split_leaf(&self, entries: &[(Vec<u8>, RecordId)], current_id: PageId) -> DbResult<(Vec<u8>, PageId)> {
        if entries.len() < 2 {
            let max_len = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            return Err(DbError::BTreeKeyTooLarge(max_len));
        }
        let mid = entries.len() / 2;
        let (left, right) = entries.split_at(mid);
        let separator = right[0].0.clone();

        {
            let mut guard = self.pool.write_page(current_id)?;
            let mut page = LeafPage::open(guard.data_mut())?;
            if !page.write_entries(left) {
                return Err(DbError::BTreeKeyTooLarge(separator.len()));
            }
        }
        let new_id = self.pool.allocate_page(PageType::BTreeLeaf)?;
        {
            let mut guard = self.pool.write_page(new_id)?;
            let mut page = LeafPage::init(guard.data_mut());
            if !page.write_entries(right) {
                return Err(DbError::BTreeKeyTooLarge(separator.len()));
            }
        }
        Ok((separator, new_id))
    }

    /// `entries`(挿入対象を含めた、`current_id`に収まりきらなかった全ての
    /// 区切りキー)を半分に分け、真ん中のキーを親へ押し上げる。
    ///
    /// Leaf Splitと違い、押し上げるキー(`entries[mid].0`)はどちらの子にも
    /// 残らない。Internal Pageのキーは「どちらの子を見るべきか」という
    /// 境界を表すだけの情報であり、Leaf Pageのキーのように行の実データと
    /// 対応する値そのものではないため、複製して残す理由が無い。
    fn split_internal(&self, entries: &[(Vec<u8>, PageId)], leftmost_child: PageId, current_id: PageId) -> DbResult<(Vec<u8>, PageId)> {
        if entries.len() < 2 {
            let max_len = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            return Err(DbError::BTreeKeyTooLarge(max_len));
        }
        let mid = entries.len() / 2;
        let separator = entries[mid].0.clone();
        let left_entries = &entries[0..mid];
        let right_leftmost = entries[mid].1;
        let right_entries = &entries[mid + 1..];

        {
            let mut guard = self.pool.write_page(current_id)?;
            let mut page = InternalPage::open(guard.data_mut())?;
            if !page.write_entries(leftmost_child, left_entries) {
                return Err(DbError::BTreeKeyTooLarge(separator.len()));
            }
        }
        let new_id = self.pool.allocate_page(PageType::BTreeInternal)?;
        {
            let mut guard = self.pool.write_page(new_id)?;
            let mut page = InternalPage::init(guard.data_mut(), right_leftmost);
            if !page.write_entries(right_leftmost, right_entries) {
                return Err(DbError::BTreeKeyTooLarge(separator.len()));
            }
        }
        Ok((separator, new_id))
    }

    /// `key_bytes`を含みうる唯一のLeaf Pageを、Rootから下って探す。
    fn find_leaf(&self, key_bytes: &[u8]) -> DbResult<PageId> {
        let mut current = self.root;
        loop {
            let guard = self.pool.read_page(current)?;
            match guard.page_type() {
                PageType::BTreeLeaf => return Ok(current),
                PageType::BTreeInternal => {
                    let view = InternalPageRef::open(guard.data())?;
                    let next = view.child_for(key_bytes);
                    drop(guard);
                    current = next;
                }
                other => return Err(unexpected_page_type(current, other)),
            }
        }
    }

    /// Rootを`new_root`へ切り替え、Metaページへ永続化する。
    fn set_root(&mut self, new_root: PageId) -> DbResult<()> {
        self.root = new_root;
        self.write_meta()
    }

    /// 現在のRootの`PageId`とキー型をMetaページへ書き込む。
    fn write_meta(&self) -> DbResult<()> {
        let mut guard = self.pool.write_page(META_PAGE_ID)?;
        let data = guard.data_mut();
        data[0..8].copy_from_slice(&self.root.0.to_le_bytes());
        data[8] = data_type_to_u8(self.key_type);
        Ok(())
    }

    /// `value`が`Value::Null`でなく、かつこのツリーの`key_type()`と一致する
    /// ことを確認する。
    fn check_key_type(&self, value: &Value) -> DbResult<()> {
        let actual = value.data_type().ok_or(DbError::NullKeyNotAllowed)?;
        if actual != self.key_type {
            return Err(DbError::BTreeKeyTypeMismatch {
                expected: self.key_type.to_string(),
                actual: actual.to_string(),
            });
        }
        Ok(())
    }

    /// このBTreeの`BufferPool`にキャッシュされているdirtyなページをすべて
    /// ディスクへ書き戻す(`HeapFile::flush`・`crate::storage::Storage::flush`
    /// と同じ役割)。
    pub fn flush(&self) -> DbResult<()> {
        self.pool.flush_all()
    }

    /// 直近の`flush`までの変更を、OSに対して実ディスクへ同期するよう要求する。
    pub fn sync(&self) -> DbResult<()> {
        self.pool.sync()
    }
}

fn unexpected_page_type(page_id: PageId, actual: PageType) -> DbError {
    DbError::CorruptPage(format!("B+Treeのページ{page_id:?}のPageTypeが不正です: {actual:?}(BTreeLeafまたはBTreeInternalが必要)"))
}

/// `entries`(キー昇順)の中で、`key`を挿入すべき位置を求める。
///
/// 同じキーがすでに存在する場合(重複キー、モジュールのドキュメント参照)は、
/// その最後の出現よりも後ろに挿入する。これにより、同じキーを持つ複数の
/// エントリは常に挿入した順序のまま並び、`entries`はどの時点でも
/// キー昇順(同じキー内では挿入順)という不変条件を保つ。
fn leaf_insert_position(entries: &[(Vec<u8>, RecordId)], key: &[u8]) -> usize {
    let mut lo = 0usize;
    let mut hi = entries.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if entries[mid].0.as_slice() <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// [`leaf_insert_position`]のInternal Page版。
fn internal_insert_position(entries: &[(Vec<u8>, PageId)], key: &[u8]) -> usize {
    let mut lo = 0usize;
    let mut hi = entries.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if entries[mid].0.as_slice() <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

/// `value`を、Leaf Page・Internal Pageの二分探索がバイト列としての大小比較
/// だけで正しく動くよう、順序を保存するバイト列へエンコードする(モジュール
/// ドキュメントの表を参照)。`Value::Null`は`DbError::NullKeyNotAllowed`。
fn encode_key(value: &Value) -> DbResult<Vec<u8>> {
    match value {
        Value::Null => Err(DbError::NullKeyNotAllowed),
        Value::Boolean(b) => Ok(vec![u8::from(*b)]),
        Value::BigInt(n) => Ok(encode_bigint(*n).to_vec()),
        Value::Text(s) => Ok(s.as_bytes().to_vec()),
    }
}

/// `i64`を、符号ビットを反転した`u64`のビッグエンディアン8バイトへ変換する
/// (モジュールドキュメントの表を参照)。
fn encode_bigint(n: i64) -> [u8; 8] {
    ((n as u64) ^ 0x8000_0000_0000_0000).to_be_bytes()
}

/// [`encode_bigint`]の逆変換。
fn decode_bigint(bytes: [u8; 8]) -> i64 {
    (u64::from_be_bytes(bytes) ^ 0x8000_0000_0000_0000) as i64
}

/// `encode_key`が生成したバイト列を`Value`へ復元する。この章の探索経路
/// (`lookup`・`insert`)はこの関数を一度も呼ばない(モジュールドキュメント
/// 参照)。テストと、将来のRange Scan(第24章)向けに用意してある。
#[cfg_attr(not(test), allow(dead_code))]
fn decode_key(key_type: DataType, bytes: &[u8]) -> DbResult<Value> {
    match key_type {
        DataType::Boolean => match bytes {
            [0] => Ok(Value::Boolean(false)),
            [1] => Ok(Value::Boolean(true)),
            other => Err(DbError::CorruptPage(format!("B+Treeのキー(BOOLEAN)が不正です: {other:?}"))),
        },
        DataType::BigInt => {
            let array: [u8; 8] = bytes
                .try_into()
                .map_err(|_| DbError::CorruptPage(format!("B+Treeのキー(BIGINT)が8バイトではありません: {}バイト", bytes.len())))?;
            Ok(Value::BigInt(decode_bigint(array)))
        }
        DataType::Text => {
            let text = String::from_utf8(bytes.to_vec()).map_err(|_| DbError::CorruptPage("B+Treeのキー(TEXT)が妥当なUTF-8ではありません".to_string()))?;
            Ok(Value::Text(text))
        }
    }
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
        other => Err(DbError::CorruptPage(format!("B+Treeのキー型コードが不正です: {other}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_manager::DiskManager;
    use crate::ids::SlotId;
    use std::collections::BTreeMap;
    use std::time::Instant;

    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        let unique = format!(
            "minidb-btree-test-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_nanos()
        );
        path.push(unique);
        path
    }

    fn open_btree(path: &std::path::Path, key_type: DataType) -> BTree {
        let disk = DiskManager::open(path).unwrap();
        BTree::create(BufferPool::new(disk, 64), key_type).unwrap()
    }

    fn rid(page: u64, slot: u16) -> RecordId {
        RecordId::new(PageId(page), SlotId(slot))
    }

    /// テスト専用の決定的な疑似乱数生成器(xorshift64)。`slotted_page`モジュールの
    /// 同名の実装と同じ理由(依存を増やさず、シードを固定して再現性を保つ)で使う。
    struct Xorshift64(u64);

    impl Xorshift64 {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
    }

    fn shuffled(n: usize, seed: u64) -> Vec<i64> {
        let mut values: Vec<i64> = (0..n as i64).collect();
        let mut rng = Xorshift64(seed);
        // Fisher-Yatesシャッフル。
        for i in (1..values.len()).rev() {
            let j = (rng.next() as usize) % (i + 1);
            values.swap(i, j);
        }
        values
    }

    #[test]
    fn key_encoding_round_trips() {
        for n in [i64::MIN, i64::MIN + 1, -1, 0, 1, 42, i64::MAX] {
            let encoded = encode_key(&Value::BigInt(n)).unwrap();
            assert_eq!(decode_key(DataType::BigInt, &encoded).unwrap(), Value::BigInt(n));
        }
        assert!(encode_key(&Value::BigInt(-1)).unwrap() < encode_key(&Value::BigInt(1)).unwrap());
        assert!(encode_key(&Value::BigInt(i64::MIN)).unwrap() < encode_key(&Value::BigInt(i64::MAX)).unwrap());

        for s in ["", "a", "ab", "abc", "b", "東京"] {
            let encoded = encode_key(&Value::Text(s.to_string())).unwrap();
            assert_eq!(decode_key(DataType::Text, &encoded).unwrap(), Value::Text(s.to_string()));
        }
        assert!(encode_key(&Value::Text("ab".to_string())).unwrap() < encode_key(&Value::Text("abc".to_string())).unwrap());
        assert!(encode_key(&Value::Text("a".to_string())).unwrap() < encode_key(&Value::Text("b".to_string())).unwrap());

        assert!(encode_key(&Value::Boolean(false)).unwrap() < encode_key(&Value::Boolean(true)).unwrap());
    }

    #[test]
    fn null_key_is_rejected_by_insert_and_lookup() {
        let path = temp_path("null-key");
        let mut btree = open_btree(&path, DataType::BigInt);
        assert!(matches!(btree.insert(&Value::Null, rid(1, 0)), Err(DbError::NullKeyNotAllowed)));
        assert!(matches!(btree.lookup(&Value::Null), Err(DbError::NullKeyNotAllowed)));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn wrong_key_type_is_rejected() {
        let path = temp_path("wrong-type");
        let mut btree = open_btree(&path, DataType::BigInt);
        let err = btree.insert(&Value::Text("x".to_string()), rid(1, 0)).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTypeMismatch { .. }));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn lookup_on_empty_tree_returns_empty() {
        let path = temp_path("empty");
        let btree = open_btree(&path, DataType::BigInt);
        assert_eq!(btree.lookup(&Value::BigInt(1)).unwrap(), Vec::new());
        assert_eq!(btree.height().unwrap(), 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn insert_then_lookup_round_trips() {
        let path = temp_path("basic");
        let mut btree = open_btree(&path, DataType::BigInt);
        btree.insert(&Value::BigInt(10), rid(1, 0)).unwrap();
        btree.insert(&Value::BigInt(20), rid(1, 1)).unwrap();
        assert_eq!(btree.lookup(&Value::BigInt(10)).unwrap(), vec![rid(1, 0)]);
        assert_eq!(btree.lookup(&Value::BigInt(20)).unwrap(), vec![rid(1, 1)]);
        assert_eq!(btree.lookup(&Value::BigInt(30)).unwrap(), Vec::new());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn duplicate_keys_are_all_returned_by_lookup() {
        let path = temp_path("duplicates");
        let mut btree = open_btree(&path, DataType::BigInt);
        btree.insert(&Value::BigInt(5), rid(1, 0)).unwrap();
        btree.insert(&Value::BigInt(5), rid(1, 1)).unwrap();
        btree.insert(&Value::BigInt(5), rid(2, 0)).unwrap();
        let mut found = btree.lookup(&Value::BigInt(5)).unwrap();
        found.sort_by_key(|r| (r.page_id.0, r.slot_id.0));
        assert_eq!(found, vec![rid(1, 0), rid(1, 1), rid(2, 0)]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn boolean_keys_work() {
        let path = temp_path("boolean");
        let mut btree = open_btree(&path, DataType::Boolean);
        btree.insert(&Value::Boolean(false), rid(1, 0)).unwrap();
        btree.insert(&Value::Boolean(true), rid(1, 1)).unwrap();
        assert_eq!(btree.lookup(&Value::Boolean(false)).unwrap(), vec![rid(1, 0)]);
        assert_eq!(btree.lookup(&Value::Boolean(true)).unwrap(), vec![rid(1, 1)]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn text_keys_preserve_lexicographic_order() {
        let path = temp_path("text");
        let mut btree = open_btree(&path, DataType::Text);
        for (i, word) in ["banana", "apple", "cherry", "ab", "abc"].iter().enumerate() {
            btree.insert(&Value::Text(word.to_string()), rid(1, i as u16)).unwrap();
        }
        for (i, word) in ["banana", "apple", "cherry", "ab", "abc"].iter().enumerate() {
            assert_eq!(btree.lookup(&Value::Text(word.to_string())).unwrap(), vec![rid(1, i as u16)]);
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn ascending_insert_then_lookup_all() {
        let path = temp_path("ascending");
        let mut btree = open_btree(&path, DataType::BigInt);
        let n = 2_000i64;
        for i in 0..n {
            btree.insert(&Value::BigInt(i), rid(1, (i % 1000) as u16)).unwrap();
        }
        for i in 0..n {
            assert_eq!(btree.lookup(&Value::BigInt(i)).unwrap(), vec![rid(1, (i % 1000) as u16)], "key={i}");
        }
        assert!(btree.height().unwrap() > 1, "2000件挿入すればRoot Splitが起きているはず");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn descending_insert_then_lookup_all() {
        let path = temp_path("descending");
        let mut btree = open_btree(&path, DataType::BigInt);
        let n = 2_000i64;
        for i in (0..n).rev() {
            btree.insert(&Value::BigInt(i), rid(1, (i % 1000) as u16)).unwrap();
        }
        for i in 0..n {
            assert_eq!(btree.lookup(&Value::BigInt(i)).unwrap(), vec![rid(1, (i % 1000) as u16)], "key={i}");
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn random_insert_then_lookup_all() {
        let path = temp_path("random");
        let mut btree = open_btree(&path, DataType::BigInt);
        let n = 2_000usize;
        let order = shuffled(n, 0x0ddc_0ffe_e123_4567);
        for &i in &order {
            btree.insert(&Value::BigInt(i), rid(1, (i % 1000) as u16)).unwrap();
        }
        for i in 0..n as i64 {
            assert_eq!(btree.lookup(&Value::BigInt(i)).unwrap(), vec![rid(1, (i % 1000) as u16)], "key={i}");
        }
        std::fs::remove_file(&path).unwrap();
    }

    /// 大きめの`TEXT`キーを使い、少ない件数でもLeaf・Internalの両方が
    /// 何段にもわたって分割される状況を作る(1ページに収まるエントリ数が
    /// 小さくなるため)。
    fn wide_key(i: usize) -> String {
        format!("{i:0>8}-{}", "x".repeat(120))
    }

    #[test]
    fn many_keys_force_multi_level_split_and_all_remain_findable() {
        let path = temp_path("multi-level-split");
        let mut btree = open_btree(&path, DataType::Text);
        let n = 1_500usize;
        let order = shuffled(n, 0x1234_5678_9abc_def0);
        for &i in &order {
            btree.insert(&Value::Text(wide_key(i as usize)), rid(1, (i % 1000) as u16)).unwrap();
        }

        let height = btree.height().unwrap();
        assert!(height >= 3, "幅の広いキーを1500件挿入すれば3段以上になるはず(実際は{height})");

        for i in 0..n {
            assert_eq!(btree.lookup(&Value::Text(wide_key(i))).unwrap(), vec![rid(1, (i % 1000) as u16)], "key={i}");
        }
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn root_split_increases_height_from_one() {
        let path = temp_path("root-split");
        let mut btree = open_btree(&path, DataType::Text);
        assert_eq!(btree.height().unwrap(), 1);

        let mut height_increased = false;
        for i in 0..500usize {
            btree.insert(&Value::Text(wide_key(i)), rid(1, 0)).unwrap();
            if btree.height().unwrap() > 1 {
                height_increased = true;
                break;
            }
        }
        assert!(height_increased, "十分な件数を挿入すればRoot Splitで高さが増えるはず");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn large_seeded_random_insert_matches_a_btreemap_model() {
        let path = temp_path("model-based");
        let mut btree = open_btree(&path, DataType::BigInt);
        let n = 5_000usize;
        let order = shuffled(n, 0x9e37_79b9_7f4a_7c15);

        let mut model: BTreeMap<i64, RecordId> = BTreeMap::new();
        for &i in &order {
            let record = rid((i as u64 / 100) + 1, (i as u16) % 100);
            btree.insert(&Value::BigInt(i), record).unwrap();
            model.insert(i, record);
        }

        for (&key, &expected) in &model {
            assert_eq!(btree.lookup(&Value::BigInt(key)).unwrap(), vec![expected], "key={key}");
        }
        // モデルに存在しないキーは空を返す。
        assert_eq!(btree.lookup(&Value::BigInt(-1)).unwrap(), Vec::new());
        assert_eq!(btree.lookup(&Value::BigInt(n as i64)).unwrap(), Vec::new());

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn reopening_the_disk_manager_preserves_the_btree_contents() {
        let path = temp_path("reopen");
        let mut inserted = Vec::new();
        {
            let disk = DiskManager::open(&path).unwrap();
            let mut btree = BTree::create(BufferPool::new(disk, 32), DataType::BigInt).unwrap();
            for i in 0..800i64 {
                let record = rid(1, (i % 1000) as u16);
                btree.insert(&Value::BigInt(i), record).unwrap();
                inserted.push((i, record));
            }
            btree.flush().unwrap();
            btree.sync().unwrap();
            // btreeはここでスコープを抜けてdropされる(closeに相当)。
        }

        let disk = DiskManager::open(&path).unwrap();
        let btree = BTree::open(BufferPool::new(disk, 32)).unwrap();
        assert_eq!(btree.key_type(), DataType::BigInt);
        for (key, expected) in inserted {
            assert_eq!(btree.lookup(&Value::BigInt(key)).unwrap(), vec![expected], "key={key}");
        }

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn single_entry_too_large_for_an_empty_page_is_rejected() {
        let path = temp_path("too-large");
        let mut btree = open_btree(&path, DataType::Text);
        let huge = "x".repeat(crate::page::PAGE_PAYLOAD_SIZE);
        let err = btree.insert(&Value::Text(huge), rid(1, 0)).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));
        std::fs::remove_file(&path).unwrap();
    }

    /// 「測って確認する」: B+Treeの`lookup`が`O(log n)`で伸びることを、
    /// 第20章の走査ベース一意性検査(`O(n)`)と同じ`n`(1,000〜16,000)で確認する。
    /// 実行環境に依存する実行時間そのものは回帰テストにしないため`#[ignore]`を
    /// 付けてあり、`cargo test --release -- --ignored --nocapture`で
    /// 手元で再現できる。
    #[test]
    #[ignore]
    fn lookup_time_grows_much_slower_than_table_size() {
        for &n in &[1_000i64, 2_000, 4_000, 8_000, 16_000] {
            let path = temp_path(&format!("lookup-bench-{n}"));
            let disk = DiskManager::open(&path).unwrap();
            let mut btree = BTree::create(BufferPool::new(disk, 256), DataType::BigInt).unwrap();
            for i in 0..n {
                btree.insert(&Value::BigInt(i), rid(1, 0)).unwrap();
            }

            let start = Instant::now();
            let found = btree.lookup(&Value::BigInt(n - 1)).unwrap();
            let elapsed = start.elapsed();
            assert_eq!(found.len(), 1);
            println!("n={n:>6} height={} elapsed={elapsed:?}", btree.height().unwrap());

            std::fs::remove_file(&path).unwrap();
        }
    }
}
