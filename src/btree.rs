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
//! # スコープ: この章(第24章)で実装するもの、しないもの
//!
//! 第23章は`BTree::create`・`BTree::open`という入口と、Point Lookup
//! (`lookup`)、Insert(`insert`)、そしてInsertが引き起こすLeaf Split・
//! Internal Split・Root Splitまでを実装した。この章ではそこへ、Leaf間リンク、
//! Range Scan(`range`)、Delete(`delete`)、キーの重複を索引自身が拒否する
//! `unique`フラグを追加する。`CREATE INDEX`・Index Maintenanceといった
//! `Storage`・`Database`との結線は`crate::storage`・`crate::index`が担い、
//! この`BTree`自身はそれらを知らない(独立したデータ構造のまま)。
//!
//! Delete は**Lazy Delete**にとどめる。エントリをLeaf Pageから取り除くだけで、
//! ページの占有率が下がってもRedistribution(隣接ページ間でエントリを
//! 融通し合う)やMerge(隣接ページ同士を1枚にまとめる)、それに伴うRoot縮小は
//! 行わない。削除を繰り返すと、空に近いLeaf Pageが木の中に残り続ける
//! (詳しい設計上の理由は本文(book)を参照)。
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
//! # `lookup`はLeaf間リンクを使う(第24章で解消した第23章の限界)
//!
//! 第23章の`lookup`は、一致したキーが1ページ(Leaf Page)の中に収まっている
//! 限りでしか全件を返せなかった。同じキーを持つエントリが多すぎてLeaf Split
//! によって複数のLeaf Pageへ分かれてしまうと、分割後にたどり着いた1ページの
//! 中の一致だけを返し、隣接するページにはみ出した分を取りこぼしていた
//! (当時のLeaf Pageがまだ横方向のリンクを持たなかったため)。
//!
//! この章では`lookup`自身を書き換えず、[`BTree::range`](Range Scan)を
//! 呼ぶ薄いラッパーへ置き換える。`range`はLeaf間リンクを使って葉から葉へ
//! 横移動するため、一致したキーが何ページにまたがっていても取りこぼさない
//! (詳しくは本文(book)を参照)。
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
//! 固定長の値だけであり、`Data`ページと区別する構造的な理由が無い。
//!
//! 第24章で、`unique`(1バイト)を末尾に追加した。
//!
//! ```text
//! offset 0        8         9        10
//! +----------------+---------+---------+
//! | root_page_id   | key_type| unique  |
//! | (8バイト、LE)   | (1バイト)| (1バイト)|
//! +----------------+---------+---------+
//! ```
//!
//! `key_type`は`0`(`BOOLEAN`)・`1`(`BIGINT`)・`2`(`TEXT`)のいずれかで、
//! `Storage`のCatalogページが列の型を符号化するのに使ったコード(モジュール
//! `crate::storage`のドキュメント参照)と同じ割り当てにしてある。`unique`は
//! `0`または`1`で、[`BTree::insert`]がキーの重複を拒否するかどうかを表す
//! (次節「重複キーの扱い」を参照)。この章より前(第23章)に作られたファイルは
//! この1バイトを持たないため、この章のコードで`BTree::open`しようとすると
//! `unique`を読む前にバイト列が尽き、`DbError::CorruptPage`になる
//! (`crate::storage`本文が第20章から採っている、章をまたいだファイル互換性を
//! 約束しない方針を参照)。
//!
//! # 重複キーの扱い(第24章で確定)
//!
//! 第23章の時点では、`insert`は常に同じキーを何度でも受け付ける多重写像
//! だった。この章では、`BTree::create`の`unique`引数が`true`のとき、
//! `insert`は既存のキーと重複する`insert`を`DbError::BTreeUniqueViolation`
//! として拒否するようになる。`PRIMARY KEY`・`UNIQUE`列に対応する索引
//! (`crate::storage::Storage::create_index`)は必ず`unique = true`で作る。
//! この索引自身は列名を知らないため、`DbError::BTreeUniqueViolation`には
//! 列名が入らない。呼び出し側([`crate::index::check_uniqueness_with_index`])が、
//! 第20章の`DbError::PrimaryKeyViolation`・`DbError::UniqueViolation`
//! (列名つき)へ翻訳してから利用者へ返す。
//!
//! # Split中の伝播が安全である理由(第3部レビュー対応)
//!
//! [`BTree::insert`]は、葉でSplitが起きると、その結果(区切りキー・新しい
//! ページのId)を親のInternal Pageへ挿入し、親でもSplitが起きればさらに
//! その親へ……というように、下から上へSplitをRootまで伝播させる
//! (最悪の場合、木の高さに比例した回数のSplitが連鎖する)。
//!
//! [`Self::split_leaf`]・[`Self::split_internal`]それぞれの単体としての
//! 原子性(片側が収まらなければ、そのページには一切触れず
//! `DbError::BTreeKeyTooLarge`を返す)だけでは、`insert`全体の原子性は
//! 保証されない。葉のSplitがすでにディスクへ実ページとして反映された
//! **後**に、伝播の途中(親・祖父母……のいずれかの階層)のSplitが
//! `DbError::BTreeKeyTooLarge`で失敗すると、`insert`は呼び出し側へ
//! エラーを返す一方、葉レベルの変更はもう元に戻せない。しかも、この
//! 葉レベルの変更は`next_leaf`(第24章)によって左隣の葉から辿れてしまう
//! ため、`insert`が失敗を報告したはずのキーを`lookup`が1件返すという
//! 矛盾した状態が生まれる。
//!
//! この矛盾を、`insert`が伝播を開始する**前**の入力検証だけで構造的に
//! 起こりえなくする。鍵は次の2点である。
//!
//! 1. **キー長の上限を「2件収まる」水準まで引き下げる**:
//!    [`crate::btree_page::leaf_max_key_len_for_two_entries`]・
//!    [`crate::btree_page::internal_max_key_len_for_two_entries`]が計算する、
//!    「空のページに**同じ長さのキーを持つエントリを2件**書き込める」
//!    という水準を、`insert`が受け付けるキー長の上限にする
//!    ([`Self::max_key_len`])。「1件収まる」よりずっと保守的だが、
//!    後述のとおりこの余裕が伝播全体の安全性を支える。
//! 2. **分割点を件数の中央でなくバイト容量で選ぶ**:
//!    [`crate::btree_page::capacity_split_point`]が、キーを左から貪欲に
//!    詰めて`capacity`を超える直前で区切る。
//!
//! この2つを組み合わせると、以下が成り立つ。
//! `insert`はSplitのどの階層でも、**すでに収まっていたページへ、ちょうど
//! 1件のエントリを追加しようとして初めて溢れる**(葉では新しいキー、
//! それより上の階層では下の階層から押し上げられた区切りキーが、いずれも
//! 1.の上限を満たす1件だけ追加される)。溢れる前のページの合計は
//! `capacity`以下、追加される1件は1.の上限より`capacity / 2`以下なので、
//! 溢れた直後の合計は`capacity + capacity / 2`を超えない。
//! `capacity_split_point`は、左側の合計が`capacity`を超える直前で止まるため、
//! 左側は構成そのものから`capacity`に収まる。さらに、それまでに追加した
//! 最後の1件を足すと超えていたはずなので、左側の合計は
//! `capacity - (追加できなかった1件の長さ)`より大きく、その1件も1.の上限
//! (`capacity / 2`以下)を満たすため、左側の合計は`capacity / 2`より大きい。
//! したがって右側の合計は、全体(`capacity + capacity / 2`以下)から左側
//! (`capacity / 2`より大きい)を引いた`capacity`未満に収まる
//! (Internal Splitで親へ押し上げる区切りキー1件は、どちらの側にも
//! 保存されないため、この余裕はさらに広がる)。
//!
//! つまり、`insert`が最初に1.の上限でキーを検証してさえいれば、以後
//! 伝播するどのSplitも、両側が収まらずに失敗する余地が構造的に無い。
//! [`Self::split_leaf`]・[`Self::split_internal`]・[`Self::grow_new_root`]が
//! それぞれ持つ`DbError::BTreeKeyTooLarge`を返す分岐は、この不変条件が
//! 崩れた場合の保険として残すが、`insert`経由では通常到達しない。

use std::ops::Bound;

use crate::buffer_pool::BufferPool;
use crate::error::{DbError, DbResult};
use crate::ids::{PageId, RecordId};
use crate::page::{PageType, PAGE_PAYLOAD_SIZE};
use crate::types::{DataType, Value};

use crate::btree_page::{
    internal_entries_fit, internal_max_key_len_for_two_entries, internal_split_point, leaf_entries_fit,
    leaf_max_key_len_for_two_entries, leaf_split_point, InternalPage, InternalPageRef, LeafPage, LeafPageRef,
    NO_NEXT_LEAF,
};

/// Metaページ(Rootの`PageId`とキー型)の定位置。ページ0はDiskManagerのFile
/// Headerが占有しているため、空いている最初の番号を使う(`crate::storage`の
/// `CATALOG_PAGE_ID`と同じ発想)。
const META_PAGE_ID: PageId = PageId(1);

/// キーから`RecordId`の集まりを引く、ディスク上のB+Tree。
pub struct BTree {
    pool: BufferPool,
    root: PageId,
    key_type: DataType,
    /// 第24章で追加。`true`なら`insert`が既存のキーとの重複を
    /// `DbError::BTreeUniqueViolation`として拒否する(モジュールドキュメントの
    /// 「重複キーの扱い」を参照)。
    unique: bool,
}

impl BTree {
    /// `pool`が管理する新しいファイルの上に、空の`BTree`を作る。
    ///
    /// `key_type`が、以後この`BTree`が受け付けるキーの型を固定する。`unique`が
    /// `true`のツリーは、`insert`が既存のキーとの重複を拒否する索引になる
    /// (`PRIMARY KEY`・`UNIQUE`列に対応する索引は必ず`true`で作る)。
    /// Metaページ(ページ1)と、空のLeaf Page1枚(初期状態のRoot)を割り当てる。
    pub fn create(pool: BufferPool, key_type: DataType, unique: bool) -> DbResult<Self> {
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

        let btree = BTree { pool, root: root_id, key_type, unique };
        btree.write_meta()?;
        Ok(btree)
    }

    /// `pool`が管理する既存のファイルから`BTree`を復元する。
    ///
    /// Metaページ(ページ1)を読み、直近に永続化されたRootの`PageId`・キー型・
    /// `unique`フラグを復元する。
    pub fn open(pool: BufferPool) -> DbResult<Self> {
        let guard = pool.read_page(META_PAGE_ID)?;
        let data = guard.data();
        let root = PageId(u64::from_le_bytes(data[0..8].try_into().unwrap()));
        let key_type = data_type_from_u8(data[8])?;
        let unique = match data[9] {
            0 => false,
            1 => true,
            other => return Err(DbError::CorruptPage(format!("B+Treeのuniqueフラグが不正です: {other}"))),
        };
        drop(guard);
        Ok(BTree { pool, root, key_type, unique })
    }

    /// このツリーがキーの重複を拒否するかどうか。
    pub fn is_unique(&self) -> bool {
        self.unique
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
    ///
    /// 実装は[`Self::range`]の`Included(key)..=Included(key)`という薄い
    /// ラッパーである。Leaf間リンクを使うため、一致したキーが複数のLeaf
    /// Pageへまたがっていても取りこぼさない(モジュールドキュメントを参照)。
    pub fn lookup(&self, key: &Value) -> DbResult<Vec<RecordId>> {
        self.range(Bound::Included(key), Bound::Included(key))?.map(|entry| entry.map(|(_, rid)| rid)).collect()
    }

    /// `lower`(下限)から`upper`(上限)までの範囲に含まれるキーを、昇順に
    /// `(Value, RecordId)`として返すイテレータを作る。
    ///
    /// `Bound::Included`・`Bound::Excluded`(境界の開閉)・`Bound::Unbounded`
    /// (その側に制限を課さない)は`std::ops::Bound`をそのまま使う。
    /// `Bound::Included(&Value::Null)`・`Bound::Excluded(&Value::Null)`は
    /// `DbError::NullKeyNotAllowed`、境界の値がこのツリーの`key_type()`と
    /// 異なる型なら`DbError::BTreeKeyTypeMismatch`を返す(`Bound::Unbounded`は
    /// 値を持たないためどちらの検査も受けない)。
    ///
    /// 開始位置となる葉をRootから1回だけ`find_leaf`(または、下限が
    /// `Unbounded`なら[`Self::leftmost_leaf`])で探し、以後は
    /// [`crate::btree_page::LeafPageRef::next_leaf`]が指す右隣の葉を
    /// たどるだけで進む。Rootへ戻る必要が無いのは、B+Treeの全データが
    /// 葉に、かつキー順に並んでいるという性質(第23章)による。
    pub fn range<'a>(&'a self, lower: Bound<&Value>, upper: Bound<&Value>) -> DbResult<RangeScan<'a>> {
        let lower_bytes = self.encode_bound(lower)?;
        let upper_bytes = self.encode_bound(upper)?;

        // `Included`は一致の最初の葉から出発する必要がある(同じキーが複数の
        // 葉にまたがる場合、それより左を取りこぼさないため)。`Excluded`は
        // `key`そのものより後ろへ進みたいだけなので、一致の最後の葉
        // (`find_leaf`、点検索と同じ探索)から出発し、そのページ内で`key`を
        // 追い越す位置まで前進すれば足りる(下の`start_index`を参照)。
        let start_leaf = match &lower_bytes {
            Bound::Unbounded => self.leftmost_leaf()?,
            Bound::Included(k) => self.find_leaf_for_lower_bound(k)?,
            Bound::Excluded(k) => self.find_leaf(k)?,
        };
        let start_index = {
            let guard = self.pool.read_page(start_leaf)?;
            let view = LeafPageRef::open(guard.data())?;
            match &lower_bytes {
                Bound::Unbounded => 0,
                Bound::Included(k) => match view.find(k) {
                    Ok(mut i) => {
                        while i > 0 && view.key(i - 1) == k.as_slice() {
                            i -= 1;
                        }
                        i
                    }
                    Err(i) => i,
                },
                Bound::Excluded(k) => match view.find(k) {
                    Ok(mut hi) => {
                        while hi + 1 < view.entry_count() && view.key(hi + 1) == k.as_slice() {
                            hi += 1;
                        }
                        hi + 1
                    }
                    Err(i) => i,
                },
            }
        };

        Ok(RangeScan { pool: &self.pool, key_type: self.key_type, upper: upper_bytes, current: Some((start_leaf, start_index)) })
    }

    /// `bound`(境界の値)をキーのバイト列へエンコードする。`Bound::Unbounded`は
    /// 値を持たないため`check_key_type`・`encode_key`のどちらも呼ばない。
    fn encode_bound(&self, bound: Bound<&Value>) -> DbResult<Bound<Vec<u8>>> {
        match bound {
            Bound::Unbounded => Ok(Bound::Unbounded),
            Bound::Included(v) => {
                self.check_key_type(v)?;
                Ok(Bound::Included(encode_key(v)?))
            }
            Bound::Excluded(v) => {
                self.check_key_type(v)?;
                Ok(Bound::Excluded(encode_key(v)?))
            }
        }
    }

    /// Rootから常に`leftmost_child`をたどり、木の中で最も左のLeaf Pageに
    /// たどり着く。[`Self::height`]と同じ経路だが、葉のPageIdそのものが
    /// 欲しい`range`(下限が`Unbounded`の場合)から使う。
    fn leftmost_leaf(&self) -> DbResult<PageId> {
        let mut current = self.root;
        loop {
            let guard = self.pool.read_page(current)?;
            match guard.page_type() {
                PageType::BTreeLeaf => return Ok(current),
                PageType::BTreeInternal => {
                    let view = InternalPageRef::open(guard.data())?;
                    let next = view.leftmost_child();
                    drop(guard);
                    current = next;
                }
                other => return Err(unexpected_page_type(current, other)),
            }
        }
    }

    /// `key`と`rid`の対応を1件削除する(Lazy Delete)。
    ///
    /// [`Self::find_leaf_for_lower_bound`]で`key`と一致する**最初**の葉から
    /// 探索を始め、`key`と`rid`の両方が一致するエントリが見つかるまで
    /// `next_leaf`を右へたどる。一致する`(key, rid)`が見つかって削除できたら
    /// `true`、そもそも存在しなければ`false`を返す。同じキーに複数の
    /// `RecordId`が対応している場合、削除するのは`rid`が一致する1件だけである。
    ///
    /// 同じキーを持つエントリがLeaf Splitによって複数のLeaf Pageへ
    /// またがっている場合、`rid`は探索経路(どちらの子へ降りるか)に一切
    /// 使われないため、`rid`だけを見て「このキーはどの葉にあるか」を
    /// 決めることはできない。`find_leaf`(一致の最後の葉を返す)から探索を
    /// 始めると、対象の`RecordId`がそれより左の葉にある場合に見つけられない
    /// (`lookup`が第24章でLeaf間リンクを使うよう書き換えられた理由と同じ)。
    /// そのため`delete`も`find_leaf_for_lower_bound`で一致の最初の葉から
    /// 出発し、右隣の葉のキーが`key`と一致しなくなる(または`next_leaf`が
    /// 尽きる)まで走査する。走査の対象になる葉が(先行する`delete`で)空に
    /// なっていても、それだけでは「このキーの残りが右の葉にもう無い」とは
    /// 判断できないため、空の葉はそのまま素通りして次の葉へ進む。
    ///
    /// **Redistribution・Merge・Root縮小は行わない**。エントリを取り除いた
    /// 結果、Leaf Pageの占有率がどれだけ下がっても、隣接するページと
    /// 融通し合ったり1枚にまとめたりしない(モジュールドキュメントの
    /// 「スコープ」を参照)。木の形(高さ、ページ数)は`delete`によって
    /// 縮む方向には変化しない。
    ///
    /// `key`が`Value::Null`なら`DbError::NullKeyNotAllowed`、このツリーの
    /// `key_type()`と異なる型なら`DbError::BTreeKeyTypeMismatch`を返す。
    pub fn delete(&mut self, key: &Value, rid: RecordId) -> DbResult<bool> {
        self.check_key_type(key)?;
        let key_bytes = encode_key(key)?;

        let mut leaf_id = self.find_leaf_for_lower_bound(&key_bytes)?;
        loop {
            let (mut entries, next_leaf) = {
                let guard = self.pool.read_page(leaf_id)?;
                let view = LeafPageRef::open(guard.data())?;
                (view.entries(), view.next_leaf())
            };

            if let Some(pos) = entries.iter().position(|(k, r)| k.as_slice() == key_bytes.as_slice() && *r == rid) {
                entries.remove(pos);
                let mut guard = self.pool.write_page(leaf_id)?;
                let mut page = LeafPage::open(guard.data_mut())?;
                let fits = page.write_entries(&entries);
                debug_assert!(fits, "エントリを取り除くだけの書き込みが収まらないのは、write_entriesの実装が壊れている場合に限る");
                return Ok(true);
            }

            // このページで見つからなかった。空の葉(先行するdeleteが空にした)、
            // あるいは末尾のキーがまだ`key`以下のページであれば、`key`の
            // エントリが右隣の葉に続いている可能性が消えないため、
            // `next_leaf`へ進む。`find_leaf_for_lower_bound`はLeaf Splitの
            // 境界次第で、`key`と等しい区切りキーを持つ葉より1つ左の葉を
            // 返すことがある(区切りキーは分割後の右側の葉にしか複製されない
            // ため、そちらの葉には`key`のエントリが1件も無い)。この場合も
            // 末尾のキーは`key`未満なので、下の条件で正しく右隣へ進む。
            // 末尾のキーが`key`を追い越しているページまで来たら、これより
            // 右に`key`のエントリが残っている余地は無い。
            let might_continue = entries.is_empty() || entries.last().unwrap().0.as_slice() <= key_bytes.as_slice();
            if !might_continue || next_leaf == NO_NEXT_LEAF {
                return Ok(false);
            }
            leaf_id = next_leaf;
        }
    }

    /// このツリーが`insert`で受け付けるキーの、エンコード後のバイト長の
    /// 上限(第3部レビュー対応)。
    ///
    /// 「空のLeaf Page(またはInternal Page)に、この長さのキーを持つ
    /// エントリを1件収められる」ではなく、**2件**収められる水準に固定して
    /// ある。この余裕が、多段Splitの伝播が構造的に`DbError::BTreeKeyTooLarge`
    /// へ到達しないことを保証する(モジュールドキュメントの「Split中の
    /// 伝播が安全である理由」を参照)。Leaf・Internalの両方で2件収まる
    /// ことを保証する必要があるため、両者のうち小さいほうの上限を使う。
    fn max_key_len(&self) -> usize {
        leaf_max_key_len_for_two_entries(PAGE_PAYLOAD_SIZE).min(internal_max_key_len_for_two_entries(PAGE_PAYLOAD_SIZE))
    }

    /// `key`を1件挿入しようとしたときに、[`Self::max_key_len`]を超えて
    /// いないかどうかを、実際には何も書き換えずに判定する。
    ///
    /// [`crate::storage::Storage::check_indexes_accept_row`]が、複数の索引を
    /// 横断して1行を挿入・更新する前に「どの索引でもこのキーが収まる」ことを
    /// 確認するために使う。索引の更新は`Storage::index_insert_row`が対象と
    /// なる索引を1つずつ順に`insert`していく形であり、事前にこの検査を
    /// 挟まないと、複数ある索引のうち途中の1つで`DbError::BTreeKeyTooLarge`が
    /// 起きたとき、それより前に更新済みの索引だけが新しい行を指し、Heapの
    /// 行そのものはすでに書き込まれている(あるいは書き換わっている)という
    /// 不整合が生まれる(第3部レビューで指摘された)。
    ///
    /// [`Self::insert`]自身も、伝播を開始する前にこの上限で`key`を検証する
    /// (この関数と同じ判定を内部で行う)。この検査を通過した`key`は、
    /// `insert`が引き起こす多段のLeaf・Internal Splitのどの階層でも
    /// `DbError::BTreeKeyTooLarge`にならないことが構造的に保証されているため
    /// (モジュールドキュメントを参照)、この関数はもう「よくある失敗を
    /// 早期に防ぐ主防御」ではなく、`insert`が返しうるエラーをHeapへの
    /// 書き込みより前に完全に予測する検査そのものである。
    ///
    /// `key`が`Value::Null`なら`DbError::NullKeyNotAllowed`、このツリーの
    /// `key_type()`と異なる型なら`DbError::BTreeKeyTypeMismatch`を返す。
    pub fn check_key_fits(&self, key: &Value) -> DbResult<()> {
        self.check_key_type(key)?;
        let key_bytes = encode_key(key)?;
        if key_bytes.len() <= self.max_key_len() {
            Ok(())
        } else {
            Err(DbError::BTreeKeyTooLarge(key_bytes.len()))
        }
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
    /// `key`のエンコード後のバイト長が[`Self::max_key_len`]を超える場合は
    /// `DbError::BTreeKeyTooLarge`を返す。このツリーが`unique`(第24章)なら、
    /// `key`がすでに存在する場合に`DbError::BTreeUniqueViolation`を返す。
    ///
    /// この検査を`key_bytes`の算出直後、木を下り始める**前**に行うことが、
    /// 多段Splitの伝播全体を原子的にする(モジュールドキュメント「Split中の
    /// 伝播が安全である理由」を参照)。伝播の途中(葉のSplitが済んだ後)で
    /// この検査を行っても、すでに葉レベルの変更をディスクへ反映してしまった
    /// 後では手遅れである。
    pub fn insert(&mut self, key: &Value, rid: RecordId) -> DbResult<()> {
        self.check_key_type(key)?;
        if self.unique && !self.lookup(key)?.is_empty() {
            return Err(DbError::BTreeUniqueViolation);
        }
        let key_bytes = encode_key(key)?;
        if key_bytes.len() > self.max_key_len() {
            return Err(DbError::BTreeKeyTooLarge(key_bytes.len()));
        }

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
    ///
    /// 第24章で、Leaf間リンク(`next_leaf`)の繋ぎ直しが加わった。`current_id`が
    /// 元々指していた右隣(`old_next`)を新しいページ(`new_id`)へ引き継ぎ、
    /// `current_id`自身の`next_leaf`は`new_id`を指すよう書き換える。この2行を
    /// 忘れると、分割の前後で「`current_id`の次は`old_next`」というリンクが
    /// 新しいページを飛び越したまま残り、`new_id`に移ったエントリへRange Scan
    /// (`range`)がたどり着けなくなる。
    ///
    /// **エラーを返す場合は既存の木を一切変更しない**。`left`・`right`の
    /// どちらかがページに収まらない場合、[`leaf_entries_fit`]による事前検査
    /// だけで判定し、`current_id`の書き換えも新しいページの確保も行わずに
    /// `DbError::BTreeKeyTooLarge`を返す。この検査を怠って`left`を先に
    /// `current_id`へ書いてしまうと、そのあとで`right`が収まらないと
    /// 判明した時点では元の`current_id`のエントリはもう失われており、
    /// 呼び出し元([`Self::insert`])へエラーを返しても木を元の状態へ
    /// 戻す手段が無い(この非対称な失敗が第3部レビューで指摘された)。
    fn split_leaf(&self, entries: &[(Vec<u8>, RecordId)], current_id: PageId) -> DbResult<(Vec<u8>, PageId)> {
        if entries.len() < 2 {
            let max_len = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            return Err(DbError::BTreeKeyTooLarge(max_len));
        }
        // 件数の中央(`entries.len() / 2`)ではなく、バイト容量を基準に分割点を
        // 選ぶ(モジュールドキュメント「Split中の伝播が安全である理由」を参照)。
        let mid = leaf_split_point(PAGE_PAYLOAD_SIZE, entries);
        let (left, right) = entries.split_at(mid);
        let separator = right[0].0.clone();

        if !leaf_entries_fit(PAGE_PAYLOAD_SIZE, left) || !leaf_entries_fit(PAGE_PAYLOAD_SIZE, right) {
            return Err(DbError::BTreeKeyTooLarge(separator.len()));
        }

        // ここから先は、両側とも収まることが確定しているため、以下の書き込みは
        // (プール自体のI/Oエラーを除けば)失敗しない。
        let old_next = {
            let guard = self.pool.read_page(current_id)?;
            LeafPageRef::open(guard.data())?.next_leaf()
        };
        let new_id = self.pool.allocate_page(PageType::BTreeLeaf)?;
        {
            let mut guard = self.pool.write_page(current_id)?;
            let mut page = LeafPage::open(guard.data_mut())?;
            let fits = page.write_entries(left);
            debug_assert!(fits, "事前検査(leaf_entries_fit)を通過した書き込みが失敗するのは実装が壊れている場合に限る");
            page.set_next_leaf(new_id);
        }
        {
            let mut guard = self.pool.write_page(new_id)?;
            let mut page = LeafPage::init(guard.data_mut());
            let fits = page.write_entries(right);
            debug_assert!(fits, "事前検査(leaf_entries_fit)を通過した書き込みが失敗するのは実装が壊れている場合に限る");
            page.set_next_leaf(old_next);
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
    ///
    /// [`Self::split_leaf`]と同じ理由で、**エラーを返す場合は既存の木を
    /// 一切変更しない**。`left_entries`・`right_entries`のどちらかが収まらない
    /// 場合は、[`internal_entries_fit`]による事前検査だけで判定し、
    /// `current_id`の書き換えも新しいページの確保も行わない。
    fn split_internal(&self, entries: &[(Vec<u8>, PageId)], leftmost_child: PageId, current_id: PageId) -> DbResult<(Vec<u8>, PageId)> {
        if entries.len() < 2 {
            let max_len = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
            return Err(DbError::BTreeKeyTooLarge(max_len));
        }
        // `split_leaf`と同じ理由で、件数の中央ではなくバイト容量を基準に
        // 分割点を選ぶ。`entries[mid]`自体はどちらの側にも保存されず親へ
        // 押し上げるだけなので、実際にはこの分割点の計算より右側にさらに
        // 余裕が生まれる(モジュールドキュメントの証明を参照)。
        let mid = internal_split_point(PAGE_PAYLOAD_SIZE, entries);
        let separator = entries[mid].0.clone();
        let left_entries = &entries[0..mid];
        let right_leftmost = entries[mid].1;
        let right_entries = &entries[mid + 1..];

        if !internal_entries_fit(PAGE_PAYLOAD_SIZE, left_entries) || !internal_entries_fit(PAGE_PAYLOAD_SIZE, right_entries) {
            return Err(DbError::BTreeKeyTooLarge(separator.len()));
        }

        // ここから先は、両側とも収まることが確定しているため、以下の書き込みは
        // (プール自体のI/Oエラーを除けば)失敗しない。
        {
            let mut guard = self.pool.write_page(current_id)?;
            let mut page = InternalPage::open(guard.data_mut())?;
            let fits = page.write_entries(leftmost_child, left_entries);
            debug_assert!(fits, "事前検査(internal_entries_fit)を通過した書き込みが失敗するのは実装が壊れている場合に限る");
        }
        let new_id = self.pool.allocate_page(PageType::BTreeInternal)?;
        {
            let mut guard = self.pool.write_page(new_id)?;
            let mut page = InternalPage::init(guard.data_mut(), right_leftmost);
            let fits = page.write_entries(right_leftmost, right_entries);
            debug_assert!(fits, "事前検査(internal_entries_fit)を通過した書き込みが失敗するのは実装が壊れている場合に限る");
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

    /// `key_bytes`を含みうる**最も左側**のLeaf Pageを、Rootから下って探す。
    /// [`Self::find_leaf`]との違いは
    /// [`InternalPageRef::child_for_lower_bound`]のドキュメントを参照。
    ///
    /// [`Self::range`]が下限(`Bound::Included`)の開始位置を決めるために使う。
    /// 同じキーを持つエントリが複数のLeaf Pageにまたがっている場合、
    /// `find_leaf`(既存の探索、`insert`・`lookup`の点検索が使う)は一致の
    /// 最後の葉を返すため、そこから[`crate::btree_page::LeafPageRef::next_leaf`]
    /// で右方向にしか進めないRange Scanの出発点には使えない
    /// (それより左の葉に残っている同じキーのエントリを取りこぼす)。
    fn find_leaf_for_lower_bound(&self, key_bytes: &[u8]) -> DbResult<PageId> {
        let mut current = self.root;
        loop {
            let guard = self.pool.read_page(current)?;
            match guard.page_type() {
                PageType::BTreeLeaf => return Ok(current),
                PageType::BTreeInternal => {
                    let view = InternalPageRef::open(guard.data())?;
                    let next = view.child_for_lower_bound(key_bytes);
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
        data[9] = u8::from(self.unique);
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

/// [`BTree::range`]が返すイテレータ。
///
/// 現在読んでいるLeaf Pageの`PageId`とページ内の添字(`current`)だけを保持し、
/// ページ内のエントリを読み尽くしたら`next_leaf`(第24章)が指す右隣の葉を
/// 読み込む。[`crate::heap_file::Scan`](第13章)が「現在のページと走査位置」
/// だけを持ってテーブル全体を読み進めるのと同じ設計であり、`BTree`全体を
/// 一度にメモリへ読み込むことはしない。
pub struct RangeScan<'a> {
    pool: &'a BufferPool,
    key_type: DataType,
    /// 上限。`Bound::Unbounded`ならどのキーも上限を超えない。
    upper: Bound<Vec<u8>>,
    /// 次に読むべき`(Leaf PageのId, そのページ内での添字)`。読み終えたら`None`。
    current: Option<(PageId, usize)>,
}

impl RangeScan<'_> {
    /// `key`が上限を超えているかどうか。
    fn exceeds_upper(&self, key: &[u8]) -> bool {
        match &self.upper {
            Bound::Unbounded => false,
            Bound::Included(k) => key > k.as_slice(),
            Bound::Excluded(k) => key >= k.as_slice(),
        }
    }
}

impl Iterator for RangeScan<'_> {
    type Item = DbResult<(Value, RecordId)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (leaf_id, index) = self.current?;
            let guard = match self.pool.read_page(leaf_id) {
                Ok(guard) => guard,
                Err(err) => {
                    self.current = None;
                    return Some(Err(err));
                }
            };
            let view = match LeafPageRef::open(guard.data()) {
                Ok(view) => view,
                Err(err) => {
                    self.current = None;
                    return Some(Err(err));
                }
            };

            if index < view.entry_count() {
                let key_bytes = view.key(index);
                if self.exceeds_upper(key_bytes) {
                    self.current = None;
                    return None;
                }
                let rid = view.record_id(index);
                let key_owned = key_bytes.to_vec();
                self.current = Some((leaf_id, index + 1));
                drop(guard);
                return Some(decode_key(self.key_type, &key_owned).map(|value| (value, rid)));
            }

            let next_leaf = view.next_leaf();
            drop(guard);
            if next_leaf == NO_NEXT_LEAF {
                self.current = None;
                return None;
            }
            self.current = Some((next_leaf, 0));
        }
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

/// `encode_key`が生成したバイト列を`Value`へ復元する。第23章の時点では
/// テストのためだけに存在したが、この章の[`RangeScan`]が返す`(Value,
/// RecordId)`の`Value`側を組み立てるのに使う(`lookup`・`insert`の探索経路
/// 自体は引き続きこの関数を呼ばない。バイト列のまま比較が完結するという
/// 設計は変えていない)。
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
        BTree::create(BufferPool::new(disk, 64), key_type, false).unwrap()
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
            let mut btree = BTree::create(BufferPool::new(disk, 32), DataType::BigInt, false).unwrap();
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

    /// レビュー指摘の再現条件: 既存のLeaf Pageがほぼ満杯の状態で、新しい
    /// ページ側に収まりようのない巨大キーを挿入すると`BTreeKeyTooLarge`を
    /// 返す。旧実装は元ページ(前半)を先に書き換えてから新ページ側の空き
    /// 容量を検査していたため、この場合に元ページのエントリが50%失われた
    /// まま(既存キーが半分しか参照できない状態で)エラーを返していた。
    /// 事前検査に変更した後は、エラーを返す代わりに木が一切変化しないことを
    /// 確認する。
    #[test]
    fn leaf_split_leaves_the_original_page_untouched_when_the_new_side_would_not_fit() {
        let path = temp_path("leaf-split-atomic");
        let mut btree = open_btree(&path, DataType::Text);

        let n = 200usize;
        let mut expected = Vec::new();
        for i in 0..n {
            let key = format!("{i:04}");
            let record = rid(1, i as u16);
            btree.insert(&Value::Text(key.clone()), record).unwrap();
            expected.push((key, record));
        }
        assert_eq!(btree.height().unwrap(), 1, "この件数・キー幅ではまだLeaf Splitが起きていないはず(前提が崩れている)");
        let leaf_id = btree.root_page_id();

        let huge_key = "x".repeat(crate::page::PAGE_PAYLOAD_SIZE);
        let err = btree.insert(&Value::Text(huge_key.clone()), rid(9, 9)).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        // エラーを返した以上、木は一切変わっていないはず: Root、葉の
        // PageId、Leaf間リンク、既存の全エントリがすべて元のまま。
        assert_eq!(btree.root_page_id(), leaf_id);
        assert_eq!(btree.height().unwrap(), 1);
        for (key, record) in &expected {
            assert_eq!(btree.lookup(&Value::Text(key.clone())).unwrap(), vec![*record], "key={key}");
        }
        assert_eq!(btree.lookup(&Value::Text(huge_key)).unwrap(), Vec::new(), "収まらなかったキーは挿入されていないはず");

        let guard = btree.pool.read_page(leaf_id).unwrap();
        let view = LeafPageRef::open(guard.data()).unwrap();
        assert_eq!(view.entry_count(), n, "元ページのエントリ数が変化している");
        assert_eq!(view.next_leaf(), NO_NEXT_LEAF, "分割していないので次の葉へのリンクは無いままのはず");

        std::fs::remove_file(&path).unwrap();
    }

    /// [`leaf_split_leaves_the_original_page_untouched_when_the_new_side_would_not_fit`]の
    /// Internal Page版。Root SplitでRootがInternal Pageになった直後の状態を
    /// 使い、`split_internal`(private、同一モジュール内のテストなので
    /// 直接呼べる)へ収まりようのない区切りキーを混ぜたエントリ列を渡す。
    #[test]
    fn internal_split_leaves_the_original_page_untouched_when_the_new_side_would_not_fit() {
        let path = temp_path("internal-split-atomic");
        let mut btree = open_btree(&path, DataType::Text);

        let mut i = 0usize;
        while btree.height().unwrap() == 1 {
            btree.insert(&Value::Text(wide_key(i)), rid(1, 0)).unwrap();
            i += 1;
            assert!(i < 10_000, "Root Splitが起きないまま挿入回数の上限に達した(テストの前提が崩れている)");
        }
        assert_eq!(btree.height().unwrap(), 2);
        let internal_id = btree.root_page_id();

        let (leftmost, original_entries) = {
            let guard = btree.pool.read_page(internal_id).unwrap();
            let view = InternalPageRef::open(guard.data()).unwrap();
            (view.leftmost_child(), view.entries())
        };
        assert!(!original_entries.is_empty(), "Root Split直後のRootは区切りキーを1本以上持つはず");

        // 収まりようがないほど巨大な区切りキーを先頭に混ぜる。
        // `internal_split_point`(バイト容量ベースの分割点選択)は先頭の1件を
        // 必ず左側に含めるため(1件も無い左側は意味を持たない)、この巨大な
        // キーは必ず左側(current_id側)に含まれ、事前検査で失敗する。
        let huge_key = "x".repeat(crate::page::PAGE_PAYLOAD_SIZE);
        let mut entries = original_entries.clone();
        entries.insert(0, (huge_key.into_bytes(), PageId(999_999)));

        let err = btree.split_internal(&entries, leftmost, internal_id).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        // エラーを返した以上、internal_idのページは一切変わっていないはず。
        let guard = btree.pool.read_page(internal_id).unwrap();
        let view = InternalPageRef::open(guard.data()).unwrap();
        assert_eq!(view.leftmost_child(), leftmost);
        assert_eq!(view.entries(), original_entries);
        assert_eq!(btree.root_page_id(), internal_id, "Root自体も変わっていないはず");

        std::fs::remove_file(&path).unwrap();
    }

    // ---- 第3部2巡目レビュー対応: insert全体(多段伝播)の原子性 ----

    #[test]
    fn insert_accepts_a_key_exactly_at_the_size_limit_and_rejects_one_byte_more() {
        let path = temp_path("insert-boundary");
        let mut btree = open_btree(&path, DataType::Text);
        let max_len = btree.max_key_len();

        let ok_key = "x".repeat(max_len);
        btree.insert(&Value::Text(ok_key.clone()), rid(1, 0)).unwrap();
        assert_eq!(btree.lookup(&Value::Text(ok_key)).unwrap(), vec![rid(1, 0)]);

        let too_large = "x".repeat(max_len + 1);
        let err = btree.insert(&Value::Text(too_large), rid(1, 1)).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn check_key_fits_matches_inserts_actual_size_limit() {
        let path = temp_path("check-key-fits-matches");
        let btree = open_btree(&path, DataType::Text);
        let max_len = btree.max_key_len();
        assert!(btree.check_key_fits(&Value::Text("x".repeat(max_len))).is_ok());
        assert!(matches!(btree.check_key_fits(&Value::Text("x".repeat(max_len + 1))), Err(DbError::BTreeKeyTooLarge(_))));
        std::fs::remove_file(&path).unwrap();
    }

    /// レビュー指摘の再現条件(修正後は再現しないことの確認): 上限ぎりぎりの
    /// 長さのキーを繰り返し挿入すると、1枚のLeaf・Internal Pageに収まる
    /// エントリ数がごく少数(最大でも2件)になるため、わずか数十件の挿入で
    /// Leaf SplitとInternal Splitが何段にもわたって連鎖する。この状況で
    /// `insert`が`DbError::BTreeKeyTooLarge`を一度も返さず完走し、挿入した
    /// 全キーが`lookup`で正しく引けることを確認する
    /// (`insert`が受け付ける上限を「空のページにキーが2件収まる」水準まで
    /// 落とし、分割点をバイト容量基準で選ぶことで、多段伝播のどの階層でも
    /// 容量エラーが起きない構造になっている。モジュールドキュメント
    /// 「Split中の伝播が安全である理由」を参照)。
    #[test]
    fn insert_completes_multi_level_propagation_without_error_for_keys_near_the_size_limit() {
        let path = temp_path("insert-multilevel-ok");
        let mut btree = open_btree(&path, DataType::Text);
        let key_len = btree.max_key_len();
        let n = 40usize;
        let mut expected = Vec::new();
        for i in 0..n {
            let key = format!("{i:06}{}", "x".repeat(key_len - 6));
            let record = rid(1, i as u16);
            btree.insert(&Value::Text(key.clone()), record).unwrap();
            expected.push((key, record));
        }
        let height = btree.height().unwrap();
        assert!(
            height >= 3,
            "上限ぎりぎりのキーを{n}件挿入すれば、葉もInternalも高々2件でSplitするため3段以上になるはず(実際は{height})"
        );

        for (key, record) in &expected {
            assert_eq!(btree.lookup(&Value::Text(key.clone())).unwrap(), vec![*record], "key={key}");
        }

        std::fs::remove_file(&path).unwrap();
    }

    /// `btree`のRoot(`PageId`)と、そこから到達できる全ページの生バイト列を
    /// (`PageId`昇順で)集めたスナップショット。`insert`が多段伝播の途中で
    /// エラーを返した後、木が呼び出し前と一切変わっていないことを、個々の
    /// ページの中身まで直接突き合わせて確認するために使う。
    fn snapshot_reachable_pages(btree: &BTree) -> (PageId, Vec<(PageId, Vec<u8>)>) {
        fn collect(pool: &BufferPool, id: PageId, out: &mut Vec<PageId>) {
            out.push(id);
            let guard = pool.read_page(id).unwrap();
            if guard.page_type() == PageType::BTreeInternal {
                let view = InternalPageRef::open(guard.data()).unwrap();
                let children: Vec<PageId> =
                    std::iter::once(view.leftmost_child()).chain(view.entries().into_iter().map(|(_, child)| child)).collect();
                drop(guard);
                for child in children {
                    collect(pool, child, out);
                }
            }
        }

        let mut ids = Vec::new();
        collect(&btree.pool, btree.root_page_id(), &mut ids);
        ids.sort_by_key(|id| id.0);
        ids.dedup();
        let pages = ids.iter().map(|&id| (id, btree.pool.read_page(id).unwrap().data().to_vec())).collect();
        (btree.root_page_id(), pages)
    }

    /// レビュー指摘の直接的な回帰テスト: 上限ぎりぎりのキーで葉・Internalが
    /// 何段にもわたって分割された深い木に対し、上限を1バイトでも超える
    /// キーを`insert`すると、`DbError::BTreeKeyTooLarge`を返し、Root・
    /// 到達可能な全ページの生バイト列・(したがって暗黙にLeaf間リンクも
    /// 含む)全エントリが呼び出し前と一致することを直接確認する。
    ///
    /// 修正前の`insert`は、キー長の検証を伝播の**途中**(各Splitの内部)で
    /// 行っていたため、葉のSplitがすでにコミットされた後で祖先の階層が
    /// 失敗し、`insert`はエラーを返す一方で`lookup`はそのキーを見つけて
    /// しまうという矛盾が生まれた(木の一部が親から辿れない形で残っても、
    /// Leaf間リンクだけは繋がってしまうため)。この修正後は、キー長の検証を
    /// 伝播を始める**前**に行うため、失敗した`insert`は木のどのページにも
    /// 一切触れない。
    #[test]
    fn insert_leaves_every_reachable_page_byte_identical_when_a_key_exceeds_the_limit_in_a_deep_tree() {
        let path = temp_path("insert-atomic-deep-multilevel");
        let mut btree = open_btree(&path, DataType::Text);
        let key_len = btree.max_key_len();
        for i in 0..40usize {
            let key = format!("{i:06}{}", "x".repeat(key_len - 6));
            btree.insert(&Value::Text(key), rid(1, i as u16)).unwrap();
        }
        assert!(btree.height().unwrap() >= 3, "この木は多段伝播を経て育っているはず(テストの前提が崩れている)");

        let before_snapshot = snapshot_reachable_pages(&btree);

        let too_large = "x".repeat(btree.max_key_len() + 1);
        let err = btree.insert(&Value::Text(too_large), rid(9, 9)).unwrap_err();
        assert!(matches!(err, DbError::BTreeKeyTooLarge(_)));

        let after_snapshot = snapshot_reachable_pages(&btree);
        assert_eq!(before_snapshot, after_snapshot, "Root・到達可能な全ページの中身が呼び出し前と完全に一致するはず");

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
            let mut btree = BTree::create(BufferPool::new(disk, 256), DataType::BigInt, false).unwrap();
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

    // ---- 第24章: Range Scan ----

    fn collect_range(btree: &BTree, lower: Bound<&Value>, upper: Bound<&Value>) -> Vec<(i64, RecordId)> {
        btree
            .range(lower, upper)
            .unwrap()
            .map(|entry| {
                let (value, rid) = entry.unwrap();
                let Value::BigInt(n) = value else { panic!("BIGINTキーのはず") };
                (n, rid)
            })
            .collect()
    }

    #[test]
    fn range_with_both_bounds_included() {
        let path = temp_path("range-included");
        let mut btree = open_btree(&path, DataType::BigInt);
        for i in 0..20i64 {
            btree.insert(&Value::BigInt(i), rid(1, i as u16)).unwrap();
        }
        let found = collect_range(&btree, Bound::Included(&Value::BigInt(5)), Bound::Included(&Value::BigInt(10)));
        assert_eq!(found.iter().map(|(n, _)| *n).collect::<Vec<_>>(), (5..=10).collect::<Vec<_>>());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn range_with_excluded_bounds() {
        let path = temp_path("range-excluded");
        let mut btree = open_btree(&path, DataType::BigInt);
        for i in 0..20i64 {
            btree.insert(&Value::BigInt(i), rid(1, i as u16)).unwrap();
        }
        let found = collect_range(&btree, Bound::Excluded(&Value::BigInt(5)), Bound::Excluded(&Value::BigInt(10)));
        assert_eq!(found.iter().map(|(n, _)| *n).collect::<Vec<_>>(), (6..=9).collect::<Vec<_>>());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn range_unbounded_on_both_sides_returns_everything_in_order() {
        let path = temp_path("range-unbounded");
        let mut btree = open_btree(&path, DataType::BigInt);
        let order = shuffled(500, 0x1111_2222_3333_4444);
        for &i in &order {
            btree.insert(&Value::BigInt(i), rid(1, (i % 1000) as u16)).unwrap();
        }
        let found = collect_range(&btree, Bound::Unbounded, Bound::Unbounded);
        assert_eq!(found.iter().map(|(n, _)| *n).collect::<Vec<_>>(), (0..500).collect::<Vec<_>>());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn range_one_sided_bounds() {
        let path = temp_path("range-one-sided");
        let mut btree = open_btree(&path, DataType::BigInt);
        for i in 0..20i64 {
            btree.insert(&Value::BigInt(i), rid(1, i as u16)).unwrap();
        }
        let lower_only = collect_range(&btree, Bound::Included(&Value::BigInt(15)), Bound::Unbounded);
        assert_eq!(lower_only.iter().map(|(n, _)| *n).collect::<Vec<_>>(), (15..20).collect::<Vec<_>>());

        let upper_only = collect_range(&btree, Bound::Unbounded, Bound::Excluded(&Value::BigInt(3)));
        assert_eq!(upper_only.iter().map(|(n, _)| *n).collect::<Vec<_>>(), (0..3).collect::<Vec<_>>());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn range_that_matches_nothing_is_empty() {
        let path = temp_path("range-empty");
        let mut btree = open_btree(&path, DataType::BigInt);
        for i in 0..20i64 {
            btree.insert(&Value::BigInt(i), rid(1, i as u16)).unwrap();
        }
        // 下限が上限を上回る範囲。
        let found = collect_range(&btree, Bound::Included(&Value::BigInt(100)), Bound::Included(&Value::BigInt(200)));
        assert!(found.is_empty());

        // 空の木に対する範囲検索。
        let empty_path = temp_path("range-empty-tree");
        let empty = open_btree(&empty_path, DataType::BigInt);
        assert!(collect_range(&empty, Bound::Unbounded, Bound::Unbounded).is_empty());

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&empty_path).unwrap();
    }

    /// 第23章が明示していた限界(「同じキーを持つエントリがLeaf Splitに
    /// よって複数ページへ分かれると、lookupは取りこぼす」)が、Leaf間リンクを
    /// 使う`range`(と、それを呼ぶ`lookup`)によって解消されていることを
    /// 確認する回帰テスト。幅の広い`TEXT`キーで1ページに収まるエントリ数を
    /// 減らし、同じキーを大量に挿入してLeaf Splitを強制的に起こす。
    #[test]
    fn duplicate_keys_spanning_multiple_leaves_are_no_longer_dropped() {
        let path = temp_path("dup-spanning-leaves");
        let mut btree = open_btree(&path, DataType::Text);
        let wide_value = "x".repeat(120);
        let n = 400usize;
        for i in 0..n {
            btree.insert(&Value::Text(wide_value.clone()), rid(1, (i % 1000) as u16)).unwrap();
        }
        assert!(btree.height().unwrap() >= 2, "幅の広いキーを400件挿入すればLeaf Splitが起きているはず");

        let found = btree.lookup(&Value::Text(wide_value)).unwrap();
        assert_eq!(found.len(), n, "全ページにまたがる重複キーを取りこぼさずに返すはず");

        std::fs::remove_file(&path).unwrap();
    }

    /// `std::collections::BTreeMap`をモデルとして、`range`の結果がモデルの
    /// 範囲検索(`BTreeMap::range`)と一致することを確認する。
    #[test]
    fn range_matches_a_btreemap_model() {
        let path = temp_path("range-model");
        let mut btree = open_btree(&path, DataType::BigInt);
        let n = 3_000usize;
        let order = shuffled(n, 0x5a5a_1234_9876_5432);

        let mut model: BTreeMap<i64, RecordId> = BTreeMap::new();
        for &i in &order {
            let record = rid((i as u64 / 100) + 1, (i as u16) % 100);
            btree.insert(&Value::BigInt(i), record).unwrap();
            model.insert(i, record);
        }

        let lower = 700i64;
        let upper = 2_400i64;
        let expected: Vec<(i64, RecordId)> =
            model.range(lower..=upper).map(|(&key, &record)| (key, record)).collect();
        let actual = collect_range(&btree, Bound::Included(&Value::BigInt(lower)), Bound::Included(&Value::BigInt(upper)));
        assert_eq!(actual, expected);

        std::fs::remove_file(&path).unwrap();
    }

    // ---- 第24章: unique フラグ ----

    #[test]
    fn unique_tree_rejects_a_duplicate_key() {
        let path = temp_path("unique-reject");
        let disk = DiskManager::open(&path).unwrap();
        let mut btree = BTree::create(BufferPool::new(disk, 64), DataType::BigInt, true).unwrap();
        btree.insert(&Value::BigInt(1), rid(1, 0)).unwrap();
        let err = btree.insert(&Value::BigInt(1), rid(1, 1)).unwrap_err();
        assert!(matches!(err, DbError::BTreeUniqueViolation));
        // 拒否された挿入は反映されない。
        assert_eq!(btree.lookup(&Value::BigInt(1)).unwrap(), vec![rid(1, 0)]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn non_unique_tree_still_accepts_duplicates() {
        let path = temp_path("non-unique-accept");
        let mut btree = open_btree(&path, DataType::BigInt);
        assert!(!btree.is_unique());
        btree.insert(&Value::BigInt(1), rid(1, 0)).unwrap();
        btree.insert(&Value::BigInt(1), rid(1, 1)).unwrap();
        assert_eq!(btree.lookup(&Value::BigInt(1)).unwrap().len(), 2);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn unique_flag_survives_reopen() {
        let path = temp_path("unique-reopen");
        {
            let disk = DiskManager::open(&path).unwrap();
            let mut btree = BTree::create(BufferPool::new(disk, 64), DataType::BigInt, true).unwrap();
            btree.insert(&Value::BigInt(1), rid(1, 0)).unwrap();
            btree.flush().unwrap();
            btree.sync().unwrap();
        }
        let disk = DiskManager::open(&path).unwrap();
        let mut reopened = BTree::open(BufferPool::new(disk, 64)).unwrap();
        assert!(reopened.is_unique());
        let err = reopened.insert(&Value::BigInt(1), rid(1, 9)).unwrap_err();
        assert!(matches!(err, DbError::BTreeUniqueViolation));
        std::fs::remove_file(&path).unwrap();
    }

    // ---- 第24章: Delete(Lazy Delete) ----

    #[test]
    fn delete_removes_the_matching_entry_and_lookup_no_longer_finds_it() {
        let path = temp_path("delete-basic");
        let mut btree = open_btree(&path, DataType::BigInt);
        btree.insert(&Value::BigInt(1), rid(1, 0)).unwrap();
        btree.insert(&Value::BigInt(2), rid(1, 1)).unwrap();

        assert!(btree.delete(&Value::BigInt(1), rid(1, 0)).unwrap());
        assert_eq!(btree.lookup(&Value::BigInt(1)).unwrap(), Vec::new());
        assert_eq!(btree.lookup(&Value::BigInt(2)).unwrap(), vec![rid(1, 1)]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_of_a_missing_entry_returns_false_and_changes_nothing() {
        let path = temp_path("delete-missing");
        let mut btree = open_btree(&path, DataType::BigInt);
        btree.insert(&Value::BigInt(1), rid(1, 0)).unwrap();

        assert!(!btree.delete(&Value::BigInt(1), rid(9, 9)).unwrap(), "キーは一致するがridが違う");
        assert!(!btree.delete(&Value::BigInt(42), rid(1, 0)).unwrap(), "キー自体が存在しない");
        assert_eq!(btree.lookup(&Value::BigInt(1)).unwrap(), vec![rid(1, 0)]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_removes_only_the_matching_rid_among_duplicate_keys() {
        let path = temp_path("delete-duplicate-key");
        let mut btree = open_btree(&path, DataType::BigInt);
        btree.insert(&Value::BigInt(5), rid(1, 0)).unwrap();
        btree.insert(&Value::BigInt(5), rid(1, 1)).unwrap();
        btree.insert(&Value::BigInt(5), rid(2, 0)).unwrap();

        assert!(btree.delete(&Value::BigInt(5), rid(1, 1)).unwrap());
        let mut remaining = btree.lookup(&Value::BigInt(5)).unwrap();
        remaining.sort_by_key(|r| (r.page_id.0, r.slot_id.0));
        assert_eq!(remaining, vec![rid(1, 0), rid(2, 0)]);
        std::fs::remove_file(&path).unwrap();
    }

    /// Lazy Delete: エントリが減ってもRedistribution・Mergeが起きないため、
    /// 一度大きく育った木の高さは、削除しても縮まない(モジュールドキュメント
    /// 「スコープ」を参照)。
    #[test]
    fn delete_does_not_shrink_the_tree_height() {
        let path = temp_path("delete-lazy");
        let mut btree = open_btree(&path, DataType::Text);
        let wide_key = |i: usize| format!("{i:0>8}-{}", "x".repeat(120));
        let n = 1_000usize;
        for i in 0..n {
            btree.insert(&Value::Text(wide_key(i)), rid(1, (i % 1000) as u16)).unwrap();
        }
        let height_before = btree.height().unwrap();
        assert!(height_before >= 2);

        for i in 0..n - 1 {
            assert!(btree.delete(&Value::Text(wide_key(i)), rid(1, (i % 1000) as u16)).unwrap());
        }
        // Lazy Deleteなので、ほぼ全件削除しても高さは縮まない。
        assert_eq!(btree.height().unwrap(), height_before);
        // 削除しなかった最後の1件は、削除で空になった葉が木の中に残っていても
        // 正しく引ける。
        assert_eq!(btree.lookup(&Value::Text(wide_key(n - 1))).unwrap(), vec![rid(1, ((n - 1) % 1000) as u16)]);

        std::fs::remove_file(&path).unwrap();
    }

    /// レビュー指摘の再現条件: 同じキーを持つ500件のエントリをLeaf Splitで
    /// 複数の葉へまたがらせ、先頭・中間・末尾の葉に残った`RecordId`を
    /// それぞれ削除できることを確認する。旧実装は`find_leaf`(一致の最後の
    /// 葉)から探索を始めていたため、先頭の葉にある`RecordId`を指定すると
    /// `deleted=false`のまま索引エントリが残っていた。
    #[test]
    fn delete_finds_the_target_rid_regardless_of_which_leaf_it_ended_up_in_after_duplicate_key_splits() {
        let path = temp_path("delete-spanning-leaves");
        let mut btree = open_btree(&path, DataType::Text);
        let wide_value = "x".repeat(120);
        let n = 500usize;
        let rids: Vec<RecordId> = (0..n).map(|i| rid(1, i as u16)).collect();
        for &r in &rids {
            btree.insert(&Value::Text(wide_value.clone()), r).unwrap();
        }
        assert!(btree.height().unwrap() >= 2, "500件の重複キーを挿入すればLeaf Splitが起きているはず");

        // 挿入順を保つ設計(`leaf_insert_position`が重複キーの最後尾へ挿入する)
        // により、先頭で挿入した`RecordId`は最も左のLeaf Pageに、末尾で
        // 挿入した`RecordId`は最も右のLeaf Pageに残る。
        let first = rids[0];
        let middle = rids[n / 2];
        let last = rids[n - 1];

        assert!(btree.delete(&Value::Text(wide_value.clone()), first).unwrap(), "先頭の葉にあるRecordIdを削除できるはず");
        assert!(btree.delete(&Value::Text(wide_value.clone()), middle).unwrap(), "中間の葉にあるRecordIdを削除できるはず");
        assert!(btree.delete(&Value::Text(wide_value.clone()), last).unwrap(), "末尾の葉にあるRecordIdを削除できるはず");
        // 一度削除したRecordIdをもう一度指定しても、もう存在しないので偽を返す。
        assert!(!btree.delete(&Value::Text(wide_value.clone()), first).unwrap());

        let mut remaining = btree.lookup(&Value::Text(wide_value.clone())).unwrap();
        remaining.sort_by_key(|r| (r.page_id.0, r.slot_id.0));
        let mut expected: Vec<RecordId> = rids.into_iter().filter(|r| *r != first && *r != middle && *r != last).collect();
        expected.sort_by_key(|r| (r.page_id.0, r.slot_id.0));
        assert_eq!(remaining, expected);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_then_lookup_and_range_agree_after_a_seeded_random_workload() {
        let path = temp_path("delete-model");
        let mut btree = open_btree(&path, DataType::BigInt);
        let n = 2_000usize;
        let order = shuffled(n, 0x2468_1357_ace0_bdf1);

        let mut model: BTreeMap<i64, RecordId> = BTreeMap::new();
        for &i in &order {
            let record = rid(1, (i % 1000) as u16);
            btree.insert(&Value::BigInt(i), record).unwrap();
            model.insert(i, record);
        }

        // 半分をシャッフル順のまま削除する。
        for &i in &order[..n / 2] {
            let record = *model.get(&i).unwrap();
            assert!(btree.delete(&Value::BigInt(i), record).unwrap());
            model.remove(&i);
        }

        for (&key, &expected) in &model {
            assert_eq!(btree.lookup(&Value::BigInt(key)).unwrap(), vec![expected], "key={key}");
        }
        for &i in &order[..n / 2] {
            assert_eq!(btree.lookup(&Value::BigInt(i)).unwrap(), Vec::new(), "削除済みのkey={i}が残っている");
        }

        let all_via_range = collect_range(&btree, Bound::Unbounded, Bound::Unbounded);
        let expected_via_model: Vec<(i64, RecordId)> = model.iter().map(|(&k, &v)| (k, v)).collect();
        assert_eq!(all_via_range, expected_via_model);

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn delete_rejects_null_and_wrong_type() {
        let path = temp_path("delete-validation");
        let mut btree = open_btree(&path, DataType::BigInt);
        assert!(matches!(btree.delete(&Value::Null, rid(1, 0)), Err(DbError::NullKeyNotAllowed)));
        assert!(matches!(
            btree.delete(&Value::Text("x".to_string()), rid(1, 0)),
            Err(DbError::BTreeKeyTypeMismatch { .. })
        ));
        std::fs::remove_file(&path).unwrap();
    }
}
