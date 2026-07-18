# 第24章 B+Tree II: 範囲検索、削除、CREATE INDEX

```console
minidb> EXPLAIN SELECT * FROM orders WHERE amount >= 100 AND amount <= 200;
QUERY PLAN
----------
Filter((amount >= 100) AND (amount <= 200))
  └─ SeqScan(orders)
(2 rows)
```

前章で作った`BTree`は、`amount = 150`のような1点だけを求める検索を`O(log n)`で解けます。
けれども`amount >= 100 AND amount <= 200`のような範囲は、まだ1件も引けません。
`BTree::lookup`が受け取るのは`Value`が1つだけで、「以上」も「以下」も表現する手段がないからです。
`EXPLAIN`の出力を見ても分かるとおり、範囲を伴う`WHERE`は今のところ`SeqScan`から一歩も動けません。

## 前章の限界

前章の`BTree`には、範囲が引けないこと以外にも2つの制限がありました。

1つ目は、削除です。
`insert`はありますが`delete`はなく、索引に一度入れたキーは取り除けません。
`DELETE FROM orders WHERE id = 42`を実行しても、まだ`orders`に対応する索引という概念自体がこの教材には存在しないため、この制限が表に出る場面もまだありませんでした。

2つ目は、SQLから使えないことです。
前章の`BTree::create`、`insert`、`lookup`はどれもRustの関数であり、`CREATE INDEX`という構文もなければ、`Database`や`Storage`との結線もありません。
`PRIMARY KEY`と`UNIQUE`の一意性検査(第20章)は、今でも行数に比例する走査のままです。

前章の`BTree`本文には、もう1つ本文中で明言していた欠陥があります。
同じキーを持つエントリがLeaf Splitによって複数のLeaf Pageへ分かれてしまうと、`lookup`は最初にたどり着いた1ページの中の一致だけを返し、隣のページにはみ出した分を取りこぼしていました。
Leaf Pageがまだ横方向のリンクを持たなかったからです。

この章では、この4つを順番に埋めていきます。
Leaf Page同士を横方向に繋ぎ、その繋がりを使って範囲検索(Range Scan)を実装し、キーの取りこぼしを解消します。
削除は、隣接ページとの再編成(RedistributionとMerge)までは踏み込まない**Lazy Delete**として実装します。
`CREATE INDEX`と`DROP INDEX`という構文を追加し、`Storage`や`Database`と結線し、既存の行から索引を作る**Index Build**と、`INSERT`、`UPDATE`、`DELETE`のたびに索引を追従させる**Index Maintenance**を実装します。
最後に、`PRIMARY KEY`と`UNIQUE`の一意性検査を索引経由に置き換え、第20章から続いていた`O(n)`の比例関係を実際に崩します。

この章ではまだ、`SELECT`の実行計画がインデックスを使うようにはしません。
`SeqScan`を`Index Scan`に置き換える最適化は第25章の仕事です。
この章が閉じるのは「`CREATE INDEX`で作った索引が、`INSERT`、`UPDATE`、`DELETE`、一意性検査から使われる」ところまでで、`SELECT`の`WHERE`はまだ索引の存在に気づきません。

## Leaf間リンク: 葉を横につなぐ

範囲検索の土台になるのは、Leaf Pageの並び方そのものです。
B+Treeは全データを葉に、かつキーの昇順で持つという性質があります(第23章)。
葉同士を左から右へ横方向に繋いでおけば、範囲の下限を含む葉さえRootから1回降りて見つければ、あとはその繋がりをたどるだけで上限までのすべてのキーを拾えます。
毎回Rootへ戻って降り直す必要がないという点が、内部ノードにも実データが散らばっているB木にはない、B+Treeだけの強みです(第23章)。

この繋がりを`next_leaf`というフィールドとしてLeaf Pageへ追加します。

```text
offset 0        2                10                   10+4n
+---------------+-----------------+--------------------+-----------------+
| entry_count   | next_leaf       | Directory (4n バイト)|   Entry Data   |
| (2バイト)     | (8バイト)       |                      |                |
+---------------+-----------------+--------------------+-----------------+
```

`next_leaf`は右隣のLeaf Pageを指す`PageId`で、右隣が無ければ`0`です。
`0`を番兵として使えるのは、ページ0が常に`DiskManager`のFile Headerに占有されていて(第11章)、Leaf Pageの`PageId`として現れることが無いからです。

```rust
pub const NO_NEXT_LEAF: PageId = PageId(0);
```

`next_leaf`を読み書きする側は、`LeafPageRef::next_leaf`と`LeafPage::set_next_leaf`です。

```rust
pub fn next_leaf(&self) -> PageId {
    PageId(read_u64(self.payload, 2))
}
```

```rust
pub fn set_next_leaf(&mut self, next: PageId) {
    self.payload[2..10].copy_from_slice(&next.0.to_le_bytes());
}
```

厄介なのは、`write_entries`(第23章)が挿入のたびに`payload`全体を`fill(0)`で作り直すことです。
何もしなければ、キーを1件挿入するだけで`next_leaf`が毎回`0`へ巻き戻ってしまいます。
そこで`write_entries`は、書き直す直前に現在の`next_leaf`を読み出しておき、新しい`payload`にもそのまま書き戻します。

```rust
pub fn write_entries(&mut self, entries: &[(Vec<u8>, RecordId)]) -> bool {
    let needed = required_len(LEAF_HEADER_SIZE, entries.iter().map(|(k, _)| k.len()), RECORD_ID_SIZE);
    if needed > self.payload.len() {
        return false;
    }

    let next_leaf = self.as_ref().next_leaf();
    self.payload.fill(0);
    self.payload[0..2].copy_from_slice(&(entries.len() as u16).to_le_bytes());
    self.payload[2..10].copy_from_slice(&next_leaf.0.to_le_bytes());
    // ...(以下、Directoryとエントリ本体を書く処理は第23章から変わらない)
    true
}
```

これで「エントリの並び替え」と「右隣への案内」が独立に扱えます。
`next_leaf`を実際に**書き換える**必要があるのは、右隣そのものが変わる場面、つまりLeaf Splitだけです。

```rust
fn split_leaf(&self, entries: &[(Vec<u8>, RecordId)], current_id: PageId) -> DbResult<(Vec<u8>, PageId)> {
    if entries.len() < 2 {
        let max_len = entries.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        return Err(DbError::BTreeKeyTooLarge(max_len));
    }
    let mid = entries.len() / 2;
    let (left, right) = entries.split_at(mid);
    let separator = right[0].0.clone();

    let new_id = self.pool.allocate_page(PageType::BTreeLeaf)?;
    let old_next = {
        let mut guard = self.pool.write_page(current_id)?;
        let mut page = LeafPage::open(guard.data_mut())?;
        let old_next = page.as_ref().next_leaf();
        if !page.write_entries(left) {
            return Err(DbError::BTreeKeyTooLarge(separator.len()));
        }
        page.set_next_leaf(new_id);
        old_next
    };
    {
        let mut guard = self.pool.write_page(new_id)?;
        let mut page = LeafPage::init(guard.data_mut());
        if !page.write_entries(right) {
            return Err(DbError::BTreeKeyTooLarge(separator.len()));
        }
        page.set_next_leaf(old_next);
    }
    Ok((separator, new_id))
}
```

分割前、`current_id`は`old_next`という右隣を持っていました。
分割後は、`current_id`(前半のエントリ)の右隣が新しくできた`new_id`(後半のエントリ)になり、`new_id`の右隣がかつての`old_next`になります。
「`current_id` → `old_next`」という1本のリンクが「`current_id` → `new_id` → `old_next`」という2本へ伸びるだけで、鎖のどこにも切れ目ができません。
`old_next`を`current_id`の`write_entries`より前に読み出しているのは、`write_entries`自身が(直前で見たとおり)そのままの`next_leaf`を保存してしまうため、上書きする前の値を確保しておく必要があるからです。

## Range Scan: 葉をたどって範囲を返す

Leaf間リンクが繋がったので、範囲検索を実装します。
下限と上限という言い方をこの章でも使いますが、境界そのものには`std::ops::Bound`をそのまま使います。
`Bound::Included`(以上または以下)、`Bound::Excluded`(より大きい、またはより小さい)、`Bound::Unbounded`(その側に制限なし)の3種類で、`col >= 100`は`Bound::Included`、`col > 100`は`Bound::Excluded`に対応します。

```rust
pub fn range<'a>(&'a self, lower: Bound<&Value>, upper: Bound<&Value>) -> DbResult<RangeScan<'a>> {
    let lower_bytes = self.encode_bound(lower)?;
    let upper_bytes = self.encode_bound(upper)?;

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
```

`Bound::Excluded`の場合と`Bound::Included`の場合とで、開始位置を探す経路を`find_leaf`と`find_leaf_for_lower_bound`に分けている点が、この章でいちばん気を遣った箇所です。
理由を、実際に踏んだ回り道込みで説明します。

最初に書いたバージョンは、境界の種類によらず`find_leaf`(前章から変わらない、Point Lookupが使う探索)で開始位置の葉を決め、その1ページの中だけを前後に走査して開始位置を決めていました。
`amount`列に同じ値がたった1ページに収まる範囲でテストしている間は、これで正しく動きます。
ところが、幅の広い`TEXT`キーを使って同じ値を400件挿入し、複数のLeaf Pageにまたがる状況を作ってテストしたところ、`lookup`(後述するとおり`range`の薄いラッパーです)が400件中25件しか返さないという失敗に行き当たりました。

原因は`find_leaf`の探索方針にありました。
`find_leaf`が使う`InternalPageRef::child_for`は、「`key`以下の区切りキーの本数」を数え、その**最後**の一致に対応する子を返します(前章)。
同じキーの重複がLeaf Splitで複数ページにまたがると、内部ページの区切りキーにも同じ値が複数回現れます。
このとき`child_for`は一貫して**最後**(挿入によって新しく育っていく側)のページを返すため、`insert`が新しい重複を追記していく行き先としては都合が良い一方、`range`の開始位置としては都合が悪いのです。
`next_leaf`は右方向にしか進めないので、`find_leaf`が一致の**最後**の葉に着地してしまうと、そこより左にある同じキーのエントリには二度とたどり着けません。

必要なのは、一致の**最初**の葉から出発することです。
そこで内部ページ探索にもう1つ、区切りキーと`key`が等しい場合の分岐だけが違う関数を用意しました。

```rust
pub fn child_for_lower_bound(&self, key: &[u8]) -> PageId {
    let mut lo = 0usize;
    let mut hi = self.entry_count();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if self.key(mid) < key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 { self.leftmost_child() } else { self.child_after(lo - 1) }
}
```

`child_for`が「`key`**以下**の区切りキーの本数」を数えるのに対し、`child_for_lower_bound`は「`key`**未満**の区切りキーの本数」を数えます。
この1文字(`<=`と`<`)の違いだけで、同じ値の区切りキーが並んでいるとき、`child_for`は最後の一致を、`child_for_lower_bound`は最初の一致を選ぶようになります。
`find_leaf_for_lower_bound`は、この`child_for_lower_bound`を使って`find_leaf`と同じ要領でRootから下るだけの、もう1つの探索経路です。

```rust
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
```

`Bound::Excluded`の側がこの新しい経路を使わずに済むのは、探しているのが「`key`を追い越した最初の位置」であって「`key`の最初の位置」ではないからです。
`find_leaf`(一致の最後の葉)に着地したうえで、そのページの中を前へ進めて`key`と等しいエントリを読み飛ばせば、その直後にあるのは`key`より大きい最初のエントリです。
ページの終端まで読み飛ばしてもまだ`key`のままなら、`RangeScan`の反復処理が次の葉へ自動的に進みます(次で見ます)。

`range`が返す`RangeScan`は、`crate::heap_file::Scan`(第13章)と同じ設計のイテレータです。
現在読んでいるLeaf Pageの`PageId`とページ内の添字だけを保持し、ページ内のエントリを読み尽くしたら`next_leaf`が指す右隣を読み込みます。

```rust
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
```

`self.pool.read_page`と`LeafPageRef::open`のどちらかが失敗した場合は、`self.current`を`None`にしてイテレータを終了させたうえで、そのエラーを1回だけ`Some(Err(...))`として返します。
壊れたページに当たった`RangeScan`が、同じエラーを繰り返し返し続けたり、エラーを無視して次のページへ進もうとしたりしないようにするためです。

上限を超えたキーに出会った時点で`None`を返して終わる(`exceeds_upper`)ため、`amount <= 200`のような上限つきの範囲は、200を超えた瞬間にそれ以上ページを読みに行きません。
`BTree`全体を舐め尽くす必要はなく、範囲に入っている件数と、範囲の外側にはみ出した最初の1件ぶんだけがコストになります。

`decode_key`(第23章、これまでテストのためだけに存在していた関数)をここで初めて実行経路の中で使います。
前章の`lookup`と`insert`はキーをバイト列のまま比較するだけで、`Value`へ戻すことは一度もありませんでした。
`range`は呼び出し側へ`Value`を返す約束のAPIなので、見つかったバイト列を`decode_key`で復元します。

### `lookup`を`range`の特別な場合として書き直す

`range`ができたことで、前章の`lookup`が抱えていた「重複キーが複数ページにまたがると取りこぼす」という限界を解消できます。
下限と上限のどちらにも同じ`key`を指定した範囲検索は、点検索そのものです。

```rust
pub fn lookup(&self, key: &Value) -> DbResult<Vec<RecordId>> {
    self.range(Bound::Included(key), Bound::Included(key))?.map(|entry| entry.map(|(_, rid)| rid)).collect()
}
```

`find_leaf_for_lower_bound`が一致の最初の葉から出発し、`RangeScan`が`next_leaf`をたどって右へ進み続けるため、同じキーが何ページに分かれていても、上限(同じ`key`)を超えるまでの全件を取りこぼさず集められます。
前章で「隣のページも見に行くという動作を実装できない」と書いた制限は、この書き換えで解消されます。

## Delete: Lazy Delete

削除を実装します。
まず、削除に何を求めないかを決めておきます。

B+Treeの標準的な削除は、エントリを取り除いた結果ページの占有率が閾値を下回ると、隣接ページからエントリを分け合う**Redistribution**や、隣接ページ同士を1枚に統合する**Merge**を行い、Merge の結果空になった親のエントリも再帰的に減らして木を縮めます。
正しく実装すれば、削除を繰り返してもページの利用効率は一定以上に保たれます。
その代わり、削除のたびに兄弟ページの状態を読みに行き、境界をまたぐ区切りキーの書き換えや、親から子への伝播をInsertのSplitと同じだけの複雑さで実装する必要があります。

この章では、そこまでは実装しません。
エントリをLeaf Pageから取り除くだけの**Lazy Delete**にとどめ、ページの占有率がどれだけ下がってもRedistributionもMergeも行いません。
削除を繰り返すと、要らないエントリが減った後も、空に近いLeaf Pageが木の中に残り続けます。
無駄なページI/Oが多少増える代わりに、実装は挿入よりずっと単純になり、次の節で必要になる「索引からエントリを1件消す」という操作を最小限のコードで用意できます。
隣接ページの再編成は、この章の演習問題に残します。

```rust
pub fn delete(&mut self, key: &Value, rid: RecordId) -> DbResult<bool> {
    self.check_key_type(key)?;
    let key_bytes = encode_key(key)?;

    let leaf_id = self.find_leaf(&key_bytes)?;
    let mut entries = {
        let guard = self.pool.read_page(leaf_id)?;
        LeafPageRef::open(guard.data())?.entries()
    };
    let Some(pos) = entries.iter().position(|(k, r)| k.as_slice() == key_bytes.as_slice() && *r == rid) else {
        return Ok(false);
    };
    entries.remove(pos);

    let mut guard = self.pool.write_page(leaf_id)?;
    let mut page = LeafPage::open(guard.data_mut())?;
    let fits = page.write_entries(&entries);
    debug_assert!(fits, "エントリを取り除くだけの書き込みが収まらないのは、write_entriesの実装が壊れている場合に限る");
    Ok(true)
}
```

`key`だけでなく`rid`も一致する条件で削除しているのは、同じキーに複数の`RecordId`が対応している場合(重複キー、第23章)に、そのうちの1件だけを消したいからです。
`find_leaf`が返す1ページの中に目的の`(key, rid)`が無ければ、そのキー自体が別ページにある可能性もありますが、削除対象は呼び出し側(次の節のIndex Maintenance)がすでに`(key, rid)`の組として正確に把握しているため、`find_leaf`の一致バイアス(前節)が問題になりません。
削除したいエントリを探すのではなく、削除したいエントリが**存在するはずの**葉を一直線に降りているだけだからです。

`write_entries`は、常にエントリが**減る**方向の書き込みなので、収まりきらずに`false`を返すことはありません。
`debug_assert!`はその前提を明文化しているだけで、実行時のコストにはなりません(releaseビルドでは消えます)。

Lazy Deleteが空に近いLeaf Pageを残しても、`lookup`と`range`の正しさそのものは崩れません。
空になった葉も、`next_leaf`のリンクとしては前後のページに正しく繋がったままだからです。
`RangeScan`(前節)は、あるページの`entry_count()`が`0`であっても、そのまま`next_leaf`をたどって次のページへ進むだけで、空のページを飛ばす特別な処理を必要としません。
崩れるのは正しさではなく効率で、削除を繰り返すほど「中身のないページを読むだけの遠回り」が増えていきます。

## `CREATE INDEX` / `DROP INDEX`

索引をSQLから作れるようにします。
構文はテーブル1個、列1個の単純な形にとどめます。

```text
CREATE [UNIQUE] INDEX <index> ON <table> (<column>)
DROP INDEX <index>
```

```console
minidb> CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT);
CREATE TABLE
minidb> CREATE UNIQUE INDEX idx_email ON users (email);
CREATE INDEX
minidb> INSERT INTO users VALUES (1, 'alice@example.com');
INSERT 1
minidb> INSERT INTO users VALUES (2, 'alice@example.com');
エラー: UNIQUE制約違反です: 列'email'の値alice@example.comが重複しています
minidb> DROP INDEX idx_email;
DROP INDEX
```

`id`列には`PRIMARY KEY`を宣言しただけで、`CREATE INDEX`を1度も書いていないのに索引が効いています。
これは、`Database::execute_create_table`が`PRIMARY KEY`と`UNIQUE`の列に対して自動で`UNIQUE`索引を作るからで、詳しくは後の節で説明します。
`idx_email`は明示的に作った索引で、`email`の重複を検出したあと、`DROP INDEX`で取り除いています。

複合キー(複数列にまたがる索引)は、前章から続く「索引キーは単一列に限る」という制約(`crate::btree::BTree`)をそのまま引き継ぎ、この章でも扱いません。
`Lexer`(第6章)に`INDEX`キーワードを1つ追加し、`Parser`(第7章)は`CREATE`と`DROP`の直後のトークンを覗き見て、既存の`CREATE TABLE`と`DROP TABLE`、新しい`CREATE INDEX`と`DROP INDEX`を振り分けます。

```rust
fn parse_create_statement(&mut self) -> DbResult<Statement> {
    match self.peek_nth_kind(1) {
        TokenKind::Keyword(Keyword::Table) => self.parse_create_table_statement().map(Statement::CreateTable),
        TokenKind::Keyword(Keyword::Index) => self.parse_create_index_statement(false).map(Statement::CreateIndex),
        TokenKind::Keyword(Keyword::Unique) => self.parse_create_index_statement(true).map(Statement::CreateIndex),
        _ => Err(self.unexpected("TABLE・INDEX・UNIQUE INDEXのいずれか")),
    }
}
```

`CREATE TABLE`が定義するのは新しいテーブル名と列名であるのに対し、`CREATE INDEX`が指定するテーブル名と列名は既存のカタログエントリを指す名前です(`DROP TABLE`の`table`と同じ立場)。
そのため`Binder`(第17章)が、テーブルと列の存在確認と、`table_id`と`column_index`への解決を担当します。

```rust
fn bind_create_index(&self, create: CreateIndexStatement) -> DbResult<BoundStatement> {
    if self.catalog.index_exists(&create.index.name) {
        return Err(self.error_at(create.index.span, format!("索引はすでに存在します: {}", create.index.name)));
    }
    let info = self
        .catalog
        .table(&create.table.name)
        .ok_or_else(|| self.error_at(create.table.span, format!("テーブルが見つかりません: {}", create.table.name)))?;
    let column_index = info.schema.index_of(&create.column.name).ok_or_else(|| {
        self.error_at(create.column.span, format!("列が見つかりません: {}", create.column.name))
    })?;

    Ok(BoundStatement::CreateIndex(BoundCreateIndex {
        index_name: create.index.name,
        table_name: info.name.clone(),
        table_id: info.id,
        column_name: create.column.name,
        column_index,
        unique: create.unique,
        span: create.span,
    }))
}
```

`index_exists`は`CatalogLookup`(第17章)に加えた新しいメソッドで、既定の実装は常に`false`を返します。

```rust
pub trait CatalogLookup {
    fn table(&self, name: &str) -> Option<&TableInfo>;

    fn index_exists(&self, _name: &str) -> bool {
        false
    }
}
```

`Database::memory`が使う`Catalog`(第9章)はこの既定のままにし、`Storage`だけが実際の索引一覧を見るよう上書きします。
`Catalog`はテーブル名と列構成の対応だけを持つ、メモリ上限定の実装であり、索引という概念そのものを持たないからです。
実行(`Database::execute_create_index`)は、メモリバックエンドに来た場合`DbError::NotImplemented`を返します。

```rust
fn execute_create_index(&mut self, create: BoundCreateIndex) -> DbResult<QueryResult> {
    match &mut self.backend {
        Backend::Memory { .. } => Err(DbError::NotImplemented(
            "CREATE INDEXはDatabase::open(ディスクバックエンド)でのみサポートされています".to_string(),
        )),
        Backend::Disk { storage } => {
            storage.create_index(&create.index_name, &create.table_name, &create.column_name, create.unique)?;
            Ok(QueryResult::command("CREATE INDEX"))
        }
    }
}
```

索引は`crate::btree::BTree`という、ページ単位でディスクI/Oを行うデータ構造の上に成り立ちます。
`Database::memory`が使う`MemStorage`(第10章)は行を`Vec<Tuple>`として直接持つだけで、ページも`BufferPool`も経由しません。
索引をメモリバックエンドへ持ち込むには、`BTree`が前提にしているページと`BufferPool`という土台をメモリ上のテーブルにも用意し直す必要があり、この章の範囲を超えます。
索引付きテーブルは、この教材では`Database::open`(永続モード)だけの機能と割り切ります。

### 索引はテーブル本体と別ファイルに置く

`Storage`(第15章)は、テーブル定義とデータページを1つのファイルへまとめて持ちます。
索引も同じファイルに同居させる案は検討しましたが、この章では採りませんでした。

`crate::btree::BTree`のMetaページは、ページ1という決め打ちの位置にあります(第23章)。
`Storage`のCatalogページも同じくページ1という決め打ちの位置にあり(第15章)、両方を同じファイルに同居させると、この2つの「ページ1」が真っ向から衝突します。
`BTree`側のMeta配置をパラメータ化して衝突を避ける道もありますが、それは前章で固めたばかりの「Metaページはページ1」という前提を、この章の都合で崩すことになります。

そこでこの章では、**索引ごとに専用のファイル**を持たせます。
`users`テーブルの`email`列に`idx_email`という索引を作ると、データベース本体のファイル(たとえば`example.db`)とは別に、`example.db.idx.idx_email`というファイルが1つ増えます。

```rust
fn index_file_path(db_path: &Path, index_name: &str) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(".idx.");
    os_string.push(index_name);
    PathBuf::from(os_string)
}
```

索引ファイルの中では、`crate::btree::BTree`が前章から変わらない前提(Metaページはページ1)のまま、何も特別扱いせずに動きます。
`Storage`が持つCatalogページには、索引の**メタデータ**(名前、テーブル、列、`unique`、キー型)だけを書き込み、索引の実データ(B+TreeのRootがどのページかといった情報)はそれぞれの索引ファイルの中に閉じておきます。
`Storage`側のカタログエンコーディング(第15章、列制約バイトを足した第20章の変更を踏襲)へ、索引の一覧を追記します。

```text
index_count: u32
indexes × index_count:
    name_len:        u16
    name:            u8 × name_len
    table_id:        u64
    column_index:    u16
    column_name_len: u16
    column_name:     u8 × column_name_len
    unique:          u8 (0 または 1)
    primary_key:     u8 (0 または 1)
    key_type:        u8 (0=BOOLEAN, 1=BIGINT, 2=TEXT)
```

`Storage::open`は、このセクションを読んだあと、記録されている索引名それぞれについて対応するファイルを開き直します。

```rust
let mut indexes = HashMap::new();
for info in decoded.indexes {
    let index_path = index_file_path(&path_buf, &info.name);
    let index_disk = DiskManager::open(&index_path)?;
    let btree = BTree::open(BufferPool::new(index_disk, DEFAULT_BUFFER_POOL_CAPACITY))?;
    indexes.insert(info.name.clone(), IndexEntry { info, btree });
}
```

索引ファイルが増えたことで見落としやすい落とし穴が1つあります。
`Storage::flush`と`Storage::sync`は、テーブル本体の`BufferPool`だけを対象にしていました(第15章)。
索引はそれぞれ独自の`BufferPool`を持つ別ファイルなので、本体だけをflushしても索引側のキャッシュはディスクへ渡らず、プロセスを再起動すると`CREATE INDEX`や後述のIndex Maintenanceで加えた変更が消えてしまいます。
実際、この章のために書いた「索引付きテーブルの再起動テスト」は、この見落としのせいで最初は失敗しました。
`Storage::flush`と`sync`を、保持している索引の数だけ`BTree::flush`と`sync`も呼ぶよう直してから、テストは通るようになりました。

```rust
pub fn flush(&self) -> DbResult<()> {
    self.pool.flush_all()?;
    for entry in self.indexes.values() {
        entry.btree.flush()?;
    }
    Ok(())
}
```

### Index Build: 既存の行から索引を作る

`CREATE INDEX`する時点で、テーブルにはすでに行が入っているかもしれません。
`Storage::create_index`は、索引ファイルを新しく作ったあと、対象テーブルを`scan`(第15章)しながら、索引化する列が`NULL`でない行だけを`BTree::insert`します。

```rust
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
```

`NULL`を持つ行を索引へ入れないのは、前章の`BTree`が`NULL`をキーとして受け付けないからです(`DbError::NullKeyNotAllowed`)。
`col IS NULL`という検索にこの索引で答えられないのは前章から続く制約で、`NULL`を持つ行を索引から除外するのはこの章のIndex BuildとIndex Maintenanceに共通する判断です。

`CREATE UNIQUE INDEX`を、すでに重複した値を持つテーブルに対して実行するとどうなるでしょうか。
`btree.insert`が`unique`違反(次の節で説明します)を返した時点でループを打ち切り、作りかけの索引ファイルを削除してから、第20章と同じ形のエラー(`DbError::UniqueViolation`または`PrimaryKeyViolation`)を返します。
索引の作成に失敗したときにファイルだけが残ってしまうと、同じ索引名で作り直そうとしたときに古いファイルの中身と新しいカタログエントリが食い違う余地が生まれるため、失敗した`CREATE INDEX`は必ずファイルも巻き戻します。

## Index Maintenance

索引は作って終わりではありません。
その後の`INSERT`、`UPDATE`、`DELETE`が、索引を対象テーブルの実際の中身とずれさせないよう追従させる必要があります。
これを`Storage`の2つのメソッドに担わせます。

```rust
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
```

`index_delete_row`も同じ形で、`btree.insert`の代わりに`btree.delete`を呼びます。
どちらも`table_id`が一致する**全索引**(`UNIQUE`かどうかを問わない)を対象にしている点が要です。
1つのテーブルに複数の索引が付いていても、行が1件変わるたびに呼び出し側(`crate::executor`)がすべての索引を個別に把握しておく必要はありません。

`executor::storage_insert`(第16章から続く、`INSERT`の実装)は、行を書き込んだ直後にこのメソッドを呼びます。

```rust
let count = planned.len();
for tuple in planned {
    let bytes = encode_tuple(schema, &tuple);
    let rid = storage.insert(table_id, &bytes)?;
    storage.index_insert_row(table_id, &tuple, rid)?;
}
Ok(count)
```

`storage_delete`も同じ形で、`storage.delete`の直後に`index_delete_row`を呼びます。

### `UPDATE`は削除してから挿入し直す

`storage_update`は少し込み入っています。
`Storage::update`(第15章)は、書き換え後の値が元のページに収まる限り同じ`RecordId`を保ちますが、収まらなければ別の場所へ挿入し、`RecordId`が変わります。
値そのものは変えていなくても、たまたま同じページに収まらなくなっただけで`RecordId`が変わるケースもあります。

索引はキーだけでなく`RecordId`も保持しているので(`crate::btree::BTree::lookup`が返すのは`RecordId`です)、`RecordId`が変わったのに索引を更新しなければ、索引は存在しない位置を指したままになります。
これを避けるため、`storage_update`は値が変わったかどうかを見ず、**常に**「更新前の値を消し、更新後の値(と、実際に確定した新しい`RecordId`)を入れ直す」という形で索引を追従させます。

```rust
let count = planned.len();
for (old_rid, old_tuple, new_tuple) in planned {
    let bytes = encode_tuple(schema, &new_tuple);
    let new_rid = storage.update(table_id, old_rid, &bytes)?.unwrap_or(old_rid);
    storage.index_delete_row(table_id, &old_tuple, old_rid)?;
    storage.index_insert_row(table_id, &new_tuple, new_rid)?;
}
Ok(count)
```

値も`RecordId`も変わらない`UPDATE`(たとえば`WHERE`に一致した行へ同じ値を書き込む文)まで、この削除→挿入を毎回行うのは無駄に見えるかもしれません。
実際に無駄ではあるのですが、「値が変わったかどうか」「`RecordId`が変わったかどうか」を先に判定してから経路を分ける実装は、判定を誤ったときに索引が古いエントリを持ち続けるという、気づきにくい形で壊れます。
この章では、常に同じ手順を踏むことで、その壊れ方そのものを構造的に無くす選択をしています。

## インデックスによるPK/UNIQUE検査

最後に、第20章から積み残していた宿題を片付けます。
`PRIMARY KEY`と`UNIQUE`の一意性検査は、テーブルの全行を読んで比較する走査ベースのままでした。

### `unique`フラグを`BTree`自身に持たせる

まず、`crate::btree::BTree`に`unique`という真偽値を持たせます。
Metaページ(第23章)の末尾に1バイト追加するだけです。

```text
offset 0        8         9        10
+----------------+---------+---------+
| root_page_id   | key_type| unique  |
| (8バイト、LE)   | (1バイト)| (1バイト)|
+----------------+---------+---------+
```

`unique`が`true`のツリーは、`insert`が既存のキーとの重複を自分で拒否します。

```rust
pub fn insert(&mut self, key: &Value, rid: RecordId) -> DbResult<()> {
    self.check_key_type(key)?;
    if self.unique && !self.lookup(key)?.is_empty() {
        return Err(DbError::BTreeUniqueViolation);
    }
    let key_bytes = encode_key(key)?;
    // ...(以下、Rootから葉までの経路を下り、Splitを伝播する処理は第23章から変わらない)
}
```

`lookup`をそのまま呼んでいるのは、前節で直した「複数ページにまたがる重複キーも取りこぼさない」という性質を、そのままこの検査にも使い回すためです。
索引自身は、自分がどのテーブルのどの列に対応するかを知りません。
`DbError::BTreeUniqueViolation`には列名が入らないため、これを第20章の`DbError::PrimaryKeyViolation`または`UniqueViolation`(列名つき)へ翻訳するのは呼び出し側の仕事です。

### 索引を使った一意性検査

`crate::index::check_uniqueness_with_index`が、`crate::constraints::check_uniqueness`(第20章)の「候補行が既存の行と重複しないか」を確かめる部分を、索引への`lookup`に置き換えます。

```rust
pub fn check_uniqueness_with_index(
    storage: &Storage,
    table_id: TableId,
    schema: &Schema,
    candidates: &[Tuple],
    exclude: &HashSet<RecordId>,
) -> crate::error::DbResult<()> {
    for (column_index, column) in schema.unique_constrained_columns() {
        let index = storage.unique_index_for_column(table_id, column_index).unwrap_or_else(|| {
            unreachable!(
                "PRIMARY KEY・UNIQUE列'{}'には第24章からCREATE TABLEが自動でUNIQUE索引を \
                 作るため、対応する索引が必ず見つかるはず",
                column.name
            )
        });
        for candidate in candidates {
            let value = candidate.get(column_index).expect("candidateはschemaと同じ列数を持つ");
            if value.is_null() {
                continue;
            }
            let matches = index.lookup(value)?;
            if matches.iter().any(|rid| !exclude.contains(rid)) {
                return Err(violation_for(column, value));
            }
        }
    }
    Ok(())
}
```

`exclude`は、`UPDATE`が「これから書き換える行自身の更新前のエントリ」を誤って重複と判定しないための除外リストです。
`INSERT`では空集合を渡し、`UPDATE`では書き換え対象の全行の**更新前**`RecordId`を渡します。
「更新前の値をまだ削除していない段階で検査する」という順序を守っているのがここの要点です。
`UPDATE users SET id = id`のような値を変えない更新も、2行が互いの`id`を交換する更新も、更新前の状態のまま検査したうえで自分自身(または交換相手)を`exclude`で除外するため、誤検出せずに通ります。

候補行**同士**の重複(同じ`INSERT`文の中の2行がどちらも新しい値で、まだ索引のどこにも登録されていない場合)は、索引への`lookup`だけでは検出できません。
この部分は第20章の`constraints::check_uniqueness`に、比較相手の行を空にした形でそのまま残します。

```rust
if schema.unique_constrained_columns().next().is_some() {
    crate::index::check_uniqueness_with_index(storage, table_id, schema, &planned, &HashSet::new())?;
    constraints::check_uniqueness(schema, std::iter::empty(), &planned)?;
}
```

`others`を空のイテレータにすると、`check_uniqueness`の「候補と既存行を比べる」ループは何もマッチさせずに素通りし、「候補同士を比べる」ループだけが実際に働きます。
既存の関数を1行も書き換えずに、2つの検査(索引経由の検査と候補同士の検査)を組み合わせられます。

### `PRIMARY KEY`と`UNIQUE`列には自動でUNIQUE索引を作る

利用者が明示的に`CREATE INDEX`しなくても、`PRIMARY KEY`と`UNIQUE`の列には自動で索引が付くようにします。
`Database::execute_create_table`が、テーブルを作った直後に対応する列だけ`create_constraint_index`を呼びます。

```rust
for (column_name, primary_key) in &constraint_columns {
    let index_name = format!("{}_{}_idx", create.table.name, column_name);
    storage.create_constraint_index(&index_name, &create.table.name, column_name, *primary_key)?;
}
```

`create_constraint_index`は、公開APIの`create_index`(`CREATE INDEX`構文が呼ぶ、常に`unique`だけを指定できる)とは別に用意した内部専用の入口で、`primary_key`まで指定できます。
`CREATE INDEX`(SQL構文)からは`PRIMARY KEY`を宣言できないので、この経路を公開APIへ混ぜ込む理由がないからです。

### 走査ベース検査の退役範囲

第20章の`constraints::check_uniqueness`は、退役しません。
`Database::memory`(メモリバックエンド)は前述のとおり索引を持たないため、引き続きこの関数だけで一意性を検査します。
退役するのは、ディスクバックエンド(`Database::open`)の`executor::storage_insert`と`storage_update`が担っていた「既存行との比較」の**呼び出し**だけで、`check_uniqueness`という関数自体は、候補行同士の比較のためにディスクバックエンドからも呼ばれ続けます。

### 測って確認する

第23章は、`BTree::lookup`が`O(log n)`で伸びることを、第20章の走査ベース検査(`O(n)`)と並べて確認しました。
この章では、その`O(n)`だった当人(`PRIMARY KEY`列を持つテーブルへの`INSERT`)が、実際に`O(log n)`へ置き換わったことを確かめます。

```console
$ cargo test --release --lib -- --ignored --nocapture insert
n=  1000 elapsed=25.31µs
n=  2000 elapsed=24.87µs
n=  4000 elapsed=27.02µs
n=  8000 elapsed=31.55µs
n= 16000 elapsed=34.90µs
```

第20章の実測(`n=1,000`で29.25µs、`n=16,000`で349.193µs)は、`n`を16倍にすると`INSERT`1件あたりのコストもほぼ16倍に伸びていました。
索引経由に置き換えた後の`INSERT`は、同じ16倍の`n`に対して35µs前後で頭打ちになり、伸び方そのものが変わっています。
索引を経由しない候補同士の比較(`constraints::check_uniqueness`)や、行そのものを`encode_tuple`して書き込むコストはまだ`n`に依存しない定数時間のままなので、この実測は「索引に置き換えた検査の部分だけが`O(n)`から`O(log n)`へ変わった」ことを裏づけています。

## テストで確認する

この章のテストは、`crate::btree`、`crate::storage`、`crate::database`の3つの層に分かれます。

`crate::btree`のテストは、Range Scanの境界(`Included`、`Excluded`、`Unbounded`の組み合わせ、空の範囲)、複数ページにまたがる重複キーが取りこぼされないことの回帰、`unique`フラグの動作、Lazy Delete後の`lookup`と`range`の一貫性を確認します。
5,000件規模のシードつき乱数列を`std::collections::BTreeMap`と突き合わせるモデルベーステスト(第23章から続く手法)は、`range`と`delete`の両方に対しても書きました。

```rust
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
```

`crate::storage`のテストは、`CREATE INDEX`のIndex Build(既存の重複を見逃さないこと)、`DROP INDEX`(索引ファイルとメタデータの両方が消えること)、`drop_table`が付随する索引もまとめて片付けること、索引付きテーブルの再起動を確認します。
`crate::database`のテストは、SQL文字列を通した統合テストです。
メモリバックエンドの走査ベース検査とディスクバックエンドの索引ベース検査が、同じ違反に対して同じ種類のエラー(`to_string()`まで一致する同一メッセージ)を返すことも確認します。

```rust
let mem_err = {
    let mut db = Database::memory();
    db.execute("CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE)").unwrap();
    db.execute("INSERT INTO users VALUES (1, 'a@example.com')").unwrap();
    expect_error(db.execute("INSERT INTO users VALUES (1, 'b@example.com')"))
};
assert!(matches!(mem_err, DbError::PrimaryKeyViolation { ref column, .. } if column == "id"));

let path = temp_db_path("index-matches-scan-pk");
let disk_err = {
    let mut db = users_pk_unique_disk_db(&path);
    db.execute("INSERT INTO users VALUES (1, 'a@example.com', 'Alice')").unwrap();
    expect_error(db.execute("INSERT INTO users VALUES (1, 'b@example.com', 'Bob')"))
};
assert!(matches!(disk_err, DbError::PrimaryKeyViolation { ref column, .. } if column == "id"));
assert_eq!(mem_err.to_string(), disk_err.to_string());
```

`UPDATE`が`RecordId`を移動させるケースは、十分に大きな`TEXT`値へ更新して元のページに収まらない状況を意図的に作り、そのあとも索引経由の一意性検査が正しく機能すること、値を変えない更新や2行の値を交換する更新が誤検出されないことを確認しています。
第3部を通して積み上げてきた回帰テストは、この章の変更後もすべて green のままです。

```console
$ cargo test --lib
test result: ok. 534 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out
```

## 演習問題

### 必須課題

1. `crate::btree::BTree::delete`はLazy Deleteで、Leaf Pageの占有率がどれだけ下がってもMergeもRedistributionも行いません。同じキーを大量に挿入してから大半を削除する`crate::btree::tests::delete_does_not_shrink_the_tree_height`は、削除後も木の高さが変わらないことを確認しています。このテストが、RedistributionとMergeを実装した後には成り立たなくなる(あるいは別の条件へ書き換える必要がある)ことを確認したうえで、隣接するLeaf Page同士でエントリを分け合うRedistributionを実装してください。占有率が下限を下回った葉について、右隣(`next_leaf`が指すページ)のエントリの一部を左へ移し、親の区切りキーを移動後の状態に合わせて書き換える必要があります。
2. 1の発展として、Redistributionでも占有率を満たせない場合(両方の葉が空に近い場合)にMergeを実装してください。2枚のLeaf Pageを1枚にまとめ、`next_leaf`を繋ぎ直し、親から不要になった区切りキーを取り除きます。親のエントリが0本になった場合、親自身も同じ基準でRedistributionやMergeの対象になり、最終的にRootまで縮む可能性があります。
3. この章の索引キーは単一列に限られています(`crate::btree::BTree`のキーは`Value`1個)。複数列の組を1つのキーとして扱う複合索引を設計してください。`encode_key`(第23章)を複数の`Value`を受け取る形へ拡張し、列ごとのエンコード結果を連結する際に列の境界を保存する必要があるかどうか(固定長の型だけを組み合わせる場合と、`TEXT`を含む場合とで答えが変わるはずです)を検討してください。実装までは求めません。

### 発展課題

1. `Storage::create_index`は、索引ごとに専用のファイルを作るという設計を採りました(モジュール冒頭の説明を参照)。この設計を、テーブル本体と同じファイルに複数の索引を同居させる設計へ書き換えるとすると、`crate::btree::BTree`のMetaページ配置(ページ1固定)をどう変更する必要があるかを設計してください。`Storage`のCatalogページに各索引のMetaページの`PageId`を記録する案と、`BTree::create`に呼び出し側が確保したMetaページの`PageId`を渡させる案の両方を検討し、それぞれが`crate::btree`のテスト(単独のファイルとして`BTree`を使う、第23章からのテスト)にどう影響するかを比較してください。
2. `crate::index::check_uniqueness_with_index`は、`PRIMARY KEY`と`UNIQUE`の列ごとに対応する`UNIQUE`索引が必ず存在するという前提のもとで`unreachable!`を使っています。テーブルを`ALTER TABLE`で後から`PRIMARY KEY`に変更できるようになったと仮定すると、この前提はどこで崩れる可能性があるか、崩れないようにするにはどこで何を検査すればよいかを検討してください(`ALTER TABLE`自体はこの教材のSQLサブセットにまだ無い機能です)。
3. `RangeScan`は、`next_leaf`をたどりながら1ページずつ`BufferPool::read_page`を呼びます。`Storage::create_index`のIndex Buildと同様に大量の行を読む場面で、`BufferPool::stats()`(第14章)を使ってヒット率を実測し、Point Lookup(`lookup`)を同じ件数繰り返す場合と比べてページI/Oの回数がどう違うかを比較してください。
