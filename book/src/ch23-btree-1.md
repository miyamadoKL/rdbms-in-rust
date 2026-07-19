# 第23章 B+Tree I: 検索、挿入、分割

```console
minidb> EXPLAIN SELECT * FROM orders WHERE id = 42;
QUERY PLAN
----------
Filter(id = 42)
  └─ SeqScan(orders)
(2 rows)
```

`orders`が1行だろうと100万行だろうと、この実行計画は変わりません。
`id`という1つの値をピンポイントで探しているのに、`SeqScan`は先頭から末尾まで全行を読み、`Filter`がその1行ずつに`id = 42`を当てて捨てていきます。

## 前章の限界

このコストは、第20章がすでに測っていました。
`PRIMARY KEY`を持つテーブルへの`INSERT`は、書き込む値が既存の行と重複していないかを`others.clone().any(...)`という線形走査で確認しており、行数を2倍にするたびに1件の`INSERT`にかかる時間もほぼ2倍になっていました。

```text
n=  1000 elapsed=29.25µs
n=  2000 elapsed=49.2µs
n=  4000 elapsed=90.551µs
n=  8000 elapsed=170.431µs
n= 16000 elapsed=349.193µs
```

`WHERE id = 42`のような1行だけを狙った`SELECT`と、`PRIMARY KEY`の一意性検査は、別々の機能に見えて実は同じ弱点を共有しています。
どちらも「ある値に一致する行を求めたい」という同じ種類の問いであり、どちらも索引を持たないテーブルではテーブル全体を見る以外に答える方法がありません。
第20章の本文は、この比例関係を崩す方法は索引を使う以外に無いと明言し、「第24章で`CREATE INDEX`が使えるようになると、この検査はB+Treeの検索(`O(log n)`)に置き換わる」と予告していました。

この章では、そのB+Treeをディスク上のデータ構造として実装します。
`CREATE INDEX`構文、既存の`SeqScan`を`Index Scan`へ置き換える最適化、一意性検査をB+Tree検索に置き換えるIndex Maintenanceは、まだ`Storage`や`Database`と結線しません。
それらは第24章と第25章の仕事です。
この章が閉じるのは「キーから`RecordId`を`O(log n)`で引ける、単体で動くデータ構造」までで、`BTree::create`、`BTree::open`、`BTree::insert`、`BTree::lookup`という低レベルAPIとテストだけで完結させます。

## なぜB+Treeなのか

二分探索木は`O(log n)`で検索できる木として最初に思いつく候補です。
しかし、この教材のデータはすべて`PAGE_SIZE`(4096バイト)単位のページに乗っていて(第11章)、ディスクI/Oも`BufferPool`のキャッシュ管理もページを単位に行われます(第14章)。
ノード1つに1組のキーと子ポインタしか持たない二分探索木をそのままページへ写すと、木の1段を降りるたびに1ページを読むことになり、行数`n`に対して木の高さは`log₂ n`に比例します。
1,000,000行なら約20段、ノードごとに別々のページを読むなら、1回の検索が最大20回のディスクI/Oを要求する計算です。

各ノードがもっと多くのキーを持てれば、1段あたりに詰め込める情報が増え、木の高さを抑えられます。
これが**B木**の発想で、1ノードに複数のキーと複数の子ポインタを持たせ、ノードのサイズをちょうど1ページに合わせます。
ノードが`k`個の子を持てるなら、木の高さは`logₖ n`に縮み、`k`が数百のオーダーになる索引では、100万行でも高さは3段から4段程度に収まります。

**B+Tree**はB木の一種で、B木と違う点が1つだけあります。
B木はどの階層のノードにも実データ(この索引では`RecordId`)を持たせますが、B+Treeは実データを**葉(Leaf)にだけ**持たせ、それより上の内部ノード(Internal)は「どちらの子を見るべきか」を示す区切りキーだけを持ちます。
この章の`BTree`がB木ではなくB+Treeを選ぶ理由は2つあります。

1つは、内部ノードが実データを持たないぶん、1ノードに詰め込める区切りキーの本数が増え、同じページサイズでもB木より扇形(fanout)が広く、木を低く保てることです。
もう1つは、全データが葉に並んでいるという構造が、範囲検索と相性が良いことです。
葉同士を横方向にリンクさせておけば(この章ではまだ実装しませんが、第24章のRange Scanが使う仕組みです)、あるキー以上の範囲を求める検索は、最初の葉さえRootから降りて見つければ、あとはリンクをたどるだけで済みます。
内部ノードにも実データが散らばっているB木では、この単純な横移動が使えません。

## 分割の不変条件

B+Treeはキーを挿入するたびに、次の3つの性質を保ち続けます。

- **整列**: どのページの中でも、エントリは常にキーの昇順に並んでいる。
- **占有率**: どのページも、`PAGE_SIZE`(この章では`PAGE_PAYLOAD_SIZE`)を超えるエントリを保持しない。収まりきらなくなったら、ページを2つに割る(Split)。
- **親子の区切りキー**: 内部ページのキー`key_i`は「`key_i`以上のキーは`key_i`の右側の子以降にある」という境界を表し、常に子の内容と矛盾しない。
- **エラーを返す場合は木を変更しない**: `insert`が`DbError::BTreeKeyTooLarge`を返す場合、呼ぶ前の木を一切変更しない。葉から根までSplitが何段にもわたって連鎖する`insert`全体についての不変条件であり、個々のSplit(`split_leaf`と`split_internal`)がそれぞれ単体として原子的であるだけでは足りない(詳しくは「キー長の上限がSplitの伝播全体を安全にする」を参照)。

この4つを保ったまま木を成長させる操作が、この章の主題である**Split**です。

## キーをバイト列へエンコードする

このSQLサブセットが対応する索引キーは単一列に限り、キーの型は`DataType`(`BOOLEAN`、`BIGINT`、`TEXT`のいずれか)を`BTree::create`の時点で1つに固定します。
複合キー(複数列の組)は、単一列の索引だけでも第23章から第25章の分量として十分な範囲であるため、この教材では扱いません。

Leaf PageとInternal Pageの探索は、キーを`Value`へ戻さずバイト列のまま大小比較できるほうが単純です。
そこでこの章のキーは、**順序を保存するバイト列**へエンコードします。

```rust
fn encode_key(value: &Value) -> DbResult<Vec<u8>> {
    match value {
        Value::Null => Err(DbError::NullKeyNotAllowed),
        Value::Boolean(b) => Ok(vec![u8::from(*b)]),
        Value::BigInt(n) => Ok(encode_bigint(*n).to_vec()),
        Value::Text(s) => Ok(s.as_bytes().to_vec()),
    }
}

fn encode_bigint(n: i64) -> [u8; 8] {
    ((n as u64) ^ 0x8000_0000_0000_0000).to_be_bytes()
}
```

`BOOLEAN`は`0`または`1`の1バイト、`TEXT`はUTF-8バイト列をそのまま使います。
Rustの`&[u8]`の`Ord`はバイト列の辞書式順序で、`"ab" < "abc"`のように短い文字列を長い文字列の接頭辞として正しく先に並べるため、長さプレフィックスを足す必要がありません。
UTF-8のバイト表現は、基本多言語面の範囲では符号点順とバイト列としての辞書式順序が一致するので、追加の変換なしにそのまま索引キーに使えます。

`BIGINT`だけひと工夫あります。
`i64`は2の補数表現なので、そのままビッグエンディアンの8バイトへ変換すると、負の数(先頭ビットが1)が正の数より大きいバイト列になり、バイト列としての大小関係と数値としての大小関係が一部で逆転します。
符号ビットを反転させてから`u64`として並べると、この逆転が`i64`全域で解消されます。

```rust
#[test]
fn key_encoding_round_trips() {
    for n in [i64::MIN, i64::MIN + 1, -1, 0, 1, 42, i64::MAX] {
        let encoded = encode_key(&Value::BigInt(n)).unwrap();
        assert_eq!(decode_key(DataType::BigInt, &encoded).unwrap(), Value::BigInt(n));
    }
    assert!(encode_key(&Value::BigInt(-1)).unwrap() < encode_key(&Value::BigInt(1)).unwrap());
    assert!(encode_key(&Value::BigInt(i64::MIN)).unwrap() < encode_key(&Value::BigInt(i64::MAX)).unwrap());
    // ...(TEXTとBOOLEANも同様に大小関係を確認する)
}
```

逆変換の`decode_key`も用意していますが、キーとキーを比較するだけの`find_leaf`、`insert_into_leaf`、`insert_into_internal`の探索経路は一度もこの関数を呼びません。
バイト列のまま比較が完結する設計にした結果、キーをいちいち`Value`へ戻すコストは、木を降りる操作にもエントリを差し込む操作にもかかりません。
`decode_key`はテストのために残してあり、第24章で追加するRange Scanが、見つかったキーのバイト列を`Value`へ戻す(呼び出し側へ返す)ためにも使うようになります。

`NULL`キーの扱いも、この時点で決めておきます。
`insert`と`lookup`に`Value::Null`を渡すと`DbError::NullKeyNotAllowed`を返します。
`NULL`は「値が分からない」ことを表すのであって、索引のキー空間上のどこかを指す値ではありません。
`col IS NULL`という検索にこの索引で答えることはできず、`NULL`を持つ行をそもそも索引へ挿入しないという判断は、`INSERT`や`UPDATE`のたびに`BTree::insert`を呼ぶかどうかを決める呼び出し側(第24章のIndex Maintenance)の責務にします。
この章の`BTree`が返すエラーは、呼び出し側がうっかり`NULL`を渡してしまった場合の防御です。

## Leaf PageとInternal Pageのレイアウト

キーがバイト列に変わったので、次はそのバイト列をページの`payload`(第11章)へどう詰めるかです。
第12章の`SlottedPage`は、挿入順のままスロットを増やし、削除された領域を`Tombstone`として残し、必要なときだけ`compact`で回収するという設計でした。
B+Treeのページはこの設計をそのまま使えません。
エントリは常にキー順に並んでいる必要があり、挿入のたびに「途中への差し込み」が起きるからです。

そこでこの章のLeaf PageとInternal Pageは、`SlottedPage`とは違う設計を採ります。
挿入のたびに**ページの全エントリを一度`Vec`へ取り出し、挿入位置を決めてから、ページ全体を1回で書き直す**という設計です。
1ページに収まるエントリ数は多くても数百程度なので、挿入のたびに全件をコピーし直すコストは無視できます。
この設計のもとでは、ページの中は常に「ディレクトリの直後からデータが隙間なく詰まっている」状態になり、`SlottedPage`が持っていた`Tombstone`や断片化は最初から発生しません。

Leaf Pageのレイアウトは次のとおりです。

```text
offset 0        2                10                   10+4n
+---------------+-----------------+--------------------+-----------------+
| entry_count   | next_leaf       | Directory (4n バイト)|   Entry Data   |
| (2バイト)     | (8バイト)       |                      |                |
+---------------+-----------------+--------------------+-----------------+
```

| フィールド | バイト数 | 内容 |
| --- | --- | --- |
| `entry_count` | 2 | エントリ数`n`(LE) |
| `next_leaf` | 8 | 右隣のLeaf Pageを指す`PageId`(LE)。無ければ`0` |
| Directory | `4 * n` | `n`個の`(key_offset: u16, key_len: u16)`の並び(LE) |

Directoryの`i`番目のエントリが指す`key_offset`から`key_len`バイトのキー、続けて`RecordId`(`page_id: u64` 8バイト + `slot_id: u16` 2バイト、計10バイト)が置かれています。
エントリはDirectoryの直後から隙間なく、Directoryと同じキー昇順で並びます。

`next_leaf`は、右隣のLeaf Pageへのポインタです。
この章では常に`0`(右隣が無いことを表す番兵)のまま埋まっており、参照する`insert`も`lookup`もまだありません。
葉同士を横方向につないでおくと、あるキー以上の範囲を求める検索は、最初の葉さえRootから降りて見つければ、あとはこのリンクをたどるだけで済みます。
このフィールドを実際に使うRange Scanと、`insert`のLeaf Splitがこのリンクを繋ぎ直す処理は第24章で実装します。

Internal Pageは、`n`本の区切りキーに対して`n + 1`本の子ページポインタを持つという非対称な形をしています。
0番目の子だけをヘッダーに固定で持たせ、残り`n`本の子は「`i`番目の区切りキーの右側にある子」として、区切りキーとセットでDirectoryに乗せます。

```text
offset 0        2                10                   10+4n
+---------------+-----------------+--------------------+-----------------+
| entry_count   | leftmost_child  | Directory (4n バイト)|   Entry Data   |
| (2バイト)     | (8バイト)       |                      |                |
+---------------+-----------------+--------------------+-----------------+
```

| フィールド | バイト数 | 内容 |
| --- | --- | --- |
| `entry_count` | 2 | 区切りキーの本数`n`(LE) |
| `leftmost_child` | 8 | 0番目の子ページを指す`PageId`(LE) |
| Directory | `4 * n` | `n`個の`(key_offset: u16, key_len: u16)`の並び(LE) |

Directoryの`i`番目のエントリが指す位置には、キーに続けて`(i + 1)`番目の子を指す`child_page_id: u64`(8バイト)が置かれます。
この章では新しい`PageType`を2つ追加し、ページの外枠(第11章)だけからLeafとInternalを区別できるようにします。

```rust
pub enum PageType {
    Meta,
    Data,
    Catalog,
    /// `crate::btree`(第23章)が使う、B+Treeの葉ページ。キーと`RecordId`の
    /// ペアを整列保持する。
    BTreeLeaf,
    /// `crate::btree`(第23章)が使う、B+Treeの内部ページ。区切りキーと
    /// 子ページへの`PageId`を保持する。
    BTreeInternal,
}
```

書き込み側の`write_entries`は、`entries`が収まりきらなければ`payload`を一切変更せず`false`を返します。

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

    let dir_end = LEAF_HEADER_SIZE + entries.len() * DIR_ENTRY_SIZE;
    let mut cursor = dir_end;
    for (i, (key, rid)) in entries.iter().enumerate() {
        let dir_base = LEAF_HEADER_SIZE + i * DIR_ENTRY_SIZE;
        self.payload[dir_base..dir_base + 2].copy_from_slice(&(cursor as u16).to_le_bytes());
        self.payload[dir_base + 2..dir_base + 4].copy_from_slice(&(key.len() as u16).to_le_bytes());

        self.payload[cursor..cursor + key.len()].copy_from_slice(key);
        cursor += key.len();
        self.payload[cursor..cursor + 8].copy_from_slice(&rid.page_id.0.to_le_bytes());
        cursor += 8;
        self.payload[cursor..cursor + 2].copy_from_slice(&rid.slot_id.0.to_le_bytes());
        cursor += 2;
    }
    true
}
```

必要バイト数を先に計算してから書き込むという順序が、`HeapFile::insert`(第13章)が`max_len_for_fresh_page`で事前にサイズを見積もっていたのと同じ理由で重要です。
書き込み始めてから足りないと分かる実装では、途中まで書きかけたページを元に戻す処理が要りますが、この順序ならその処理自体が不要になります。

`next_leaf`を`self.payload.fill(0)`の直前に読み出し、書き直した`payload`へそのまま書き戻している点に注目してください。
`write_entries`はエントリの並び替え(挿入)のたびに`payload`全体を作り直しますが、右隣のLeaf Pageへのリンクはエントリの中身とは無関係な「このページ自身がどこにあるか」という情報なので、書き換えのたびに失われては困ります。
この章では`next_leaf`が常に`0`(前節の番兵)のまま素通りするだけですが、リンク自体を繋ぎ直す操作は第24章のLeaf Splitで使います。

`open`(検証つきで開く経路)は、`SlottedPage::open`(第12章)と同じ理由でバイト範囲を検証しますが、もう1つ`SlottedPage`には無い検証を加えています。
キーが昇順に並んでいるかどうかです。

```rust
let key = &payload[key_offset..key_end];
if let Some(prev) = &previous_key {
    if prev.as_slice() > key {
        return Err(DbError::CorruptPage(format!(
            "{what}のエントリ{i}のキーが、直前のエントリのキーより小さいです(昇順に並んでいる必要があります)"
        )));
    }
}
```

この検証は`prev > key`(狭義の降順)だけを拒否し、`prev == key`(重複)は許します。
重複キーの扱いは次の節で決めます。

Leaf、Internalどちらの探索も二分探索で行うため、`entry_count()`だけを使った添字配列をいったん`Vec`へ`collect`してから`slice::binary_search_by`に渡すような実装は書きません。
それでは1回の探索がエントリ数に比例した`Vec`確保を伴い、この章が目指す「ページ内探索は`O(log n)`」という前提が崩れてしまいます。
代わりに`lo`と`hi`だけを持つ二分探索を手で書き、ページの`payload`を直接読んで比較します。

```rust
pub fn find(&self, key: &[u8]) -> Result<usize, usize> {
    let mut lo = 0usize;
    let mut hi = self.entry_count();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match self.key(mid).cmp(key) {
            std::cmp::Ordering::Equal => return Ok(mid),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    Err(lo)
}
```

## 検索: Rootから葉まで一本道を降りる

`BTree`はページ1(ページ0はDiskManagerのFile Headerが占有します)をMetaページとして使い、現在のRootの`PageId`とキー型を持たせます。
`Storage`(第15章)がCatalogページ専用に`PageType::Catalog`を新設したのとは対照的に、この章では新しいPage Typeを追加せず、既存の`PageType::Data`を転用します。
Metaページが持つ情報は「Rootの`PageId`(8バイト)」と「キー型(1バイト)」の2値だけで、複数テーブルの定義という可変長のコレクションを持っていたCatalogページとは事情が異なるからです。

```rust
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
```

`unique`は、キーの重複を`insert`自身が拒否するかどうかを決める第24章のフラグで、この章のテストはすべて`false`(重複を許す)を渡します。
`unique`の使い道が分かるまでは読み飛ばして構いません。

作りたての`BTree`は、空のLeaf Page1枚だけを持つ、高さ1の木です。
検索(`lookup`)は、Rootから葉までの経路を`find_leaf`で下ります。

```rust
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
```

`InternalPageRef::child_for`が、区切りキーとの二分探索で「`key`未満の区切りキーの本数」を数え、その本数に応じて`leftmost_child`かいずれかの`child_after(i)`を返します。

```rust
pub fn child_for(&self, key: &[u8]) -> PageId {
    let mut lo = 0usize;
    let mut hi = self.entry_count();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if self.key(mid) <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    if lo == 0 { self.leftmost_child() } else { self.child_after(lo - 1) }
}
```

葉に着いたら、その葉の中を`LeafPageRef::find`で二分探索します。
重複キーが許されている(次の節で決めます)ため、一致した1件の前後にも同じキーが続いていないかを確認してから、一致した全件をまとめて返す必要があります。

```rust
pub fn lookup(&self, key: &Value) -> DbResult<Vec<RecordId>> {
    self.range(Bound::Included(key), Bound::Included(key))?.map(|entry| entry.map(|(_, rid)| rid)).collect()
}
```

`lookup`の実体は`range`(下限と上限のどちらにも`key`を指定した範囲検索)への委譲です。
`LeafPageRef::find`で一致を1件見つけたあと、その前後に同じキーが続いていないか`lo`と`hi`で走査する処理自体は、`range`の内部(開始位置を決める部分)にあります。

この前後の走査には、同じキーを持つエントリがLeaf Splitによって複数ページへ分かれてしまうと、`find_leaf`で降りた1ページの外にまで手が届かないという限界があります。
この章のLeaf Pageはまだ横方向のリンクを持たないため、「隣のページも見に行く」という動作を実装できないからです。
この節で触れた`next_leaf`(まだ`0`で埋まっているだけの、右隣のLeaf Pageへのポインタ)を使って隣のページへ渡り歩けば、この取りこぼしは解消されます。
その仕組み(Range Scan)自体は第24章で実装します。

## 挿入とSplit: 木を成長させる

`insert`は、まずRootから葉までの経路を`find_leaf`と同じ要領で下ります。
違うのは、通過したInternal Pageの`PageId`を`path`という`Vec`へ記録しておく点です。
この`path`が、Splitが起きたときに「どのページへ区切りキーを押し上げればよいか」を教えてくれます。

```rust
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
```

### Leaf Split

葉に着いたら、その葉の全エントリを`Vec`へ取り出し、挿入位置を二分探索で決めて差し込み、`write_entries`を試します。

```rust
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
```

`leaf_insert_position`は、同じキーがすでに存在する場合、その最後の出現よりも後ろに挿入位置を返します。
これが、この章が採る**重複キーの扱い**です。
`insert`は同じキーを持つエントリを何度でも受け付ける、キーの一意性に関知しない多重写像として実装します。
`PRIMARY KEY`や`UNIQUE`の一意性検査(第20章の`crate::constraints`と同じ役割のもの)をこの索引自身に持たせる変更は、第24章でIndex Maintenanceを実装するときに扱います。

`write_entries`が`false`を返したら(収まらなかったら)、`split_leaf`を呼びます。
挿入後の全エントリをちょうど半分に割り、前半は元のページへ、後半は新しく確保したLeaf Pageへ書き直します。

```rust
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
```

`leaf_entries_fit`は、`write_entries`が内部で使っている「必要バイト数を計算して`payload`の大きさと比べる」計算だけを公開したヘルパーで、実際のページには一切触れません。
`left`と`right`のどちらか一方でも収まらなければ、`current_id`の書き換えも新しいページの確保も行わずに`DbError::BTreeKeyTooLarge`を返します。

この事前検査が無いとどうなるか考えてみます。
仮に前半(`left`)を先に`current_id`へ書いてしまってから後半(`right`)の書き込みを試みたとします。
`right`が収まらないと分かるのはその時点であり、`current_id`はすでに前半だけの内容に書き換わっています。
呼び出し元(`insert`)へエラーを返しても、この書き換えを元に戻す手段はもうありません。
「Split以前に存在していたエントリの半分が消える」という、分割の不変条件のどれとも両立しない状態が残ります。
事前検査によって、両側の`write_entries`はどちらも失敗しないと分かってから初めて実行するので、この状態は起こりえません。

`old_next`(元の`current_id`が指していた右隣)を新しいページ(`new_id`)へ引き継ぎ、`current_id`自身は`new_id`を指すよう`next_leaf`を書き換えている点が、`next_leaf`を持たなかった場合との違いです。
分割によって2枚に増えたページの間にも、分割前と同じ「キー順に並んだ横のリンク」を保ちます。

親へ押し上げる区切りキーには、後半の先頭キー(`right[0]`)をそのまま使います。
このキーは新しいLeaf Pageにも物理的にコピーされたまま残ります。
「`separator`以上のキーは新しいページにある」という区切りキーの意味と、「新しいページの最小キーが`separator`そのもの」という実際の中身が、そのまま一致するからです。

### Internal Split

`split_leaf`が返した`(区切りキー, 新しいページのId)`は、`path`から取り出した親のInternal Pageへ、Leaf Splitと同じ要領で挿入します。

```rust
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
```

収まらなければ`split_internal`です。
ここがLeaf Splitと異なる、この章で唯一非対称な箇所になります。

```rust
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
    // ...(current_idへleft_entries、新しいInternal Pageへright_entriesを書く)
    Ok((separator, new_id))
}
```

Leaf Splitは後半の先頭キーをコピーして区切りキーにしましたが、Internal Splitは真ん中のキー(`entries[mid].0`)をどちらの子にも残さず、そのまま親へ押し上げます。
Internal Pageのキーは「どちらの子を見るべきか」という境界を表すだけの情報で、Leaf Pageのキーのように行の実データと対応する値そのものではないため、複製して残す理由がありません。
この非対称性が、モジュール冒頭で決めた「分割の不変条件」の3つ目(親子の区切りキー)をLeafとInternalの両方で保ち続ける仕組みです。

`internal_entries_fit`による事前検査と、検査を通過するまで実ページに触れない構造は、`split_leaf`とまったく同じ理由です。
先に`left_entries`を`current_id`へ書いてから`right_entries`が収まらないと判明した場合、`current_id`はもう元のエントリを失っており、エラーを返しても元へ戻せません。

### Root Split

Splitの結果を押し上げる先が無くなった(`path`が空になった)ときは、Root自身が分割されたということです。
新しいInternal Pageを1枚確保し、古いRootを`leftmost_child`、Splitで生まれた新しいページを唯一の区切りキーの右側の子として、これを新しいRootに据えます。

```rust
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
```

`insert`本体は、この3つの操作(`insert_into_leaf`、`insert_into_internal`、`grow_new_root`)を、Splitが止まるまで下から上へ繰り返すだけです。

```rust
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
```

`path.pop()`が`None`を返すのは、`find_leaf`と同じ経路を下るときに1つも内部ページを通らなかった(葉がRootそのものだった)場合と、`path`を根まで使い切った場合の両方です。
どちらの場合も「これ以上押し上げる先が無い」という同じ状況であり、Root Splitで新しいRootを作る以外に取りうる道はありません。
B+Treeが常に「全ての葉が同じ深さに揃う」性質を保つのは、木を高くする操作がこのRoot Splitだけであり、あるRootから葉までの経路を1段掘り下げるとき、他のどの経路も必ず同時に1段深くなるからです。

Rootが変わったら、その`PageId`をMetaページへ書き戻します。

```rust
fn set_root(&mut self, new_root: PageId) -> DbResult<()> {
    self.root = new_root;
    self.write_meta()
}
```

この永続化を忘れると、プロセスを再起動した`BTree::open`が古いRootの`PageId`を読み込み、Root Splitで追い出されたはずの古いページを頂点として扱ってしまいます。

### キー長の上限がSplitの伝播全体を安全にする

`insert_into_leaf`、`insert_into_internal`、`grow_new_root`を下から上へ繰り返す、と書きましたが、この繰り返しの**途中**で`DbError::BTreeKeyTooLarge`が起きたらどうなるでしょうか。
葉のSplitがすでにディスクへ実ページとして反映された後に、1段上のInternal Splitが収まらずに失敗したとします。
`split_leaf`と`split_internal`はそれぞれ単体としては原子的(エラーを返す場合はそのページに一切触れない)ですが、それだけでは`insert`全体の原子性は保証されません。
葉レベルの変更はもう元に戻せないうえ、その変更は`next_leaf`(第24章で導入するLeaf間リンク)によって左隣の葉から辿れてしまうため、`insert`が失敗を報告したはずのキーを`lookup`が1件返すという矛盾した状態が生まれます。

この矛盾を、`insert`が伝播を開始する**前**の入力検証だけで構造的に起こりえなくします。
鍵は2つあります。

1つ目は、`insert`が受け付けるキー長の上限を、「空のページに**同じ長さのキーを持つエントリを1件**収められる」水準ではなく、「**2件**収められる」水準まで引き下げることです。

```rust
fn max_key_len(&self) -> usize {
    leaf_max_key_len_for_two_entries(PAGE_PAYLOAD_SIZE).min(internal_max_key_len_for_two_entries(PAGE_PAYLOAD_SIZE))
}
```

`leaf_max_key_len_for_two_entries`と`internal_max_key_len_for_two_entries`は、それぞれLeafとInternal Pageが空の状態から同じ長さのキーを2件受け入れられる上限を計算するだけの、実際のページに触れない純粋な計算です。
`insert`は、`key_bytes`を求めた直後、木を下り始める前にこの上限を検査します。

```rust
let key_bytes = encode_key(key)?;
if key_bytes.len() > self.max_key_len() {
    return Err(DbError::BTreeKeyTooLarge(key_bytes.len()));
}
```

2つ目は、`split_leaf`と`split_internal`の分割点`mid`を、件数の中央ではなくバイト容量で選ぶことです(`leaf_split_point`と`internal_split_point`、前節ですでに使いました)。

この2つを組み合わせると、次が成り立ちます。
`insert`はSplitのどの階層でも、**すでに収まっていたページへ、ちょうど1件のエントリを追加しようとして初めて溢れます**(葉では新しいキー、それより上の階層では下から押し上げられた区切りキーが、いずれも1つ目の上限を満たす1件だけ追加されます)。
溢れる前のページの合計は`capacity`(ページの容量)以下、追加される1件は1つ目の上限より`capacity / 2`以下なので、溢れた直後の合計は`capacity + capacity / 2`を超えません。
バイト容量基準の分割点は、左側の合計が`capacity`を超える直前で止まるため、左側は構成そのものから`capacity`に収まります。
さらに、それまでに追加した最後の1件を足すと超えていたはずなので、左側の合計は`capacity`から「追加できなかった1件の長さ」を引いた値より大きく、その1件も1つ目の上限(`capacity / 2`以下)を満たすため、左側の合計は`capacity / 2`より大きくなります。
したがって右側の合計は、全体(`capacity + capacity / 2`以下)から左側(`capacity / 2`より大きい)を引いた`capacity`未満に収まります(Internal Splitで親へ押し上げる区切りキー1件はどちらの側にも保存されないため、この余裕はさらに広がります)。

つまり、`insert`が最初にこの上限でキーを検証してさえいれば、以後伝播するどのSplitも、両側が収まらずに失敗する余地が構造的にありません。
`split_leaf`、`split_internal`、`grow_new_root`が持つ`DbError::BTreeKeyTooLarge`を返す分岐は、この不変条件が崩れた場合の保険として残しますが、`insert`経由では通常到達しません。

## テストで確認する

`btree`モジュールと`btree_page`モジュールのテストは、大きく3種類に分かれます。

1つ目は、ページ内レイアウトの単体テスト(`btree_page`)です。
書き込みと読み込みの往復、収まりきらない挿入が`payload`を変更せず`false`を返すこと、壊れたバイト列を`open`が`CorruptPage`として検出することを確認しています。
検証する壊れ方は、単純な範囲外オフセットや降順のキーだけではありません。
`validate`は、`write_entries`が生成する正規のレイアウト(先頭エントリの`key_offset`は必ずDirectory直後、以降の各エントリは直前のエントリの終端から始まる)そのものを検査しているため、キー領域がHeaderやDirectoryを指す配置や、複数エントリの領域が重なる配置(それぞれ`payload`の範囲には収まっているので、範囲チェックだけでは見逃してしまいます)も`CorruptPage`として拒否できることを、LeafとInternalの両方で確認しています。

2つ目は、`BTree`自体の機能テストです。
`NULL`キーやキー型の不一致が拒否されること、重複キーを挿入すると全件が`lookup`で返ること、`BOOLEAN`や`TEXT`のキーでも正しく動くことを確認したうえで、昇順、降順、ランダムな順序で挿入した場合のいずれでも、挿入した全キーが`lookup`で一致することを確認します。

```rust
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
```

Split自体が実際に起きていることは、単に「挿入したキーが後から引ける」だけでは確認できません。
ページサイズがそこそこ大きい(4080バイト)ため、`BIGINT`キーを数千件挿入しただけではLeaf Splitが数回起きるだけで終わり、Internal SplitやRoot Splitまで踏み込みません。
そこで幅の広い`TEXT`キー(128バイト程度)を使い、1ページに収まるエントリ数を意図的に減らしたテストを別に用意し、`height()`(Rootから葉までの階層数を返すメソッド)で3段以上に育っていることを確認しています。

```rust
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
```

`height()`は、Rootから常に`leftmost_child`をたどるだけの単純なメソッドです。

```rust
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
```

どの経路をたどっても同じ値になるのは、B+Treeが「全ての葉が同じ深さに揃う」性質を持つからで、これはRoot Splitについて確認した性質そのものです。
このメソッドを使い、空の木の高さが1であること、十分な件数を挿入するとその高さが実際に増えることも別途確認しています。

`split_leaf`と`split_internal`が「エラーを返す場合は木を変更しない」という不変条件を守っていることは、まずそれぞれの局所的なテストで確認します。
`leaf_split_leaves_the_original_page_untouched_when_the_new_side_would_not_fit`は、既存のLeaf Pageがほぼ満杯の状態へ、新しいページ側に収まりようのない巨大キーを挿入し、`DbError::BTreeKeyTooLarge`を受け取った後もRoot、元の葉の`PageId`、Leaf間リンク、既存の全エントリが変化していないことを検証します。
`internal_split_leaves_the_original_page_untouched_when_the_new_side_would_not_fit`はInternal Page版で、Root Splitで生まれたRoot(Internal Page)の区切りキーへ収まりようのないキーを混ぜて`split_internal`(同じモジュール内のテストなので直接呼べます)を呼び、同じ不変条件を確認します。

これらは`split_leaf`と`split_internal`という私有関数を直接呼ぶテストであり、`insert`全体(葉から根までの多段伝播)の不変条件までは検証しません。
それを検証するのが、公開APIの`insert`だけを使う一連のテストです。
`insert_accepts_a_key_exactly_at_the_size_limit_and_rejects_one_byte_more`は、`max_key_len`ちょうどの長さのキーが挿入でき、1バイトでも長いキーは拒否されることを確認します。
`insert_completes_multi_level_propagation_without_error_for_keys_near_the_size_limit`は、上限ぎりぎりの長さのキーを40件挿入し(1ページに収まるエントリ数が高々2件になるため、高さ3段以上の多段Splitが連鎖します)、`insert`が一度も`DbError::BTreeKeyTooLarge`を返さずに完走し、挿入した全キーが`lookup`で正しく引けることを確認します。
`insert_leaves_every_reachable_page_byte_identical_when_a_key_exceeds_the_limit_in_a_deep_tree`は、同じように育てた深い木に対して上限を1バイト超えるキーを`insert`し、`DbError::BTreeKeyTooLarge`を受け取った後もRootと到達可能な全ページの生バイト列が呼び出し前と完全に一致することを、個々のページの中身まで直接突き合わせて確認します。

3つ目は、`Storage`(第15章)がすでに使っている「シード固定のXorshiftで決定的な乱数列を作る」という手法を借りたモデルベーステストです。
5,000件の`BIGINT`キーをシャッフルして挿入しながら、同じキーと`RecordId`を`std::collections::BTreeMap`にも積んでおき、全キーについて`lookup`の結果が`BTreeMap`の記録と一致することを確認します。

```rust
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
```

`std`の`BTreeMap`はメモリ上の実装で、ページも`BufferPool`もSplitも持ちません。
この教材の`BTree`と実装の中身は何も共有していない、独立に実装された「正解」を突き合わせる相手として使っています。
5,000件という件数は、Leaf SplitとInternal Splitの両方を何度も引き起こすには十分な量です。

最後に、`Storage`と同じ再起動の確認も行います。
挿入後に`flush`と`sync`を呼んでからファイルを閉じ、`BTree::open`で開き直しても、Metaページに書き戻しておいたRootの`PageId`からすべてのキーがそのまま引けることを確認しています。

```console
$ cargo test --lib btree
test result: ok. 23 passed; 0 failed; 1 ignored; 0 measured; 472 filtered out; finished in 1.33s
```

### 測って確認する

第20章の走査ベースの一意性検査は、行数`n`に比例して`INSERT`1件あたりのコストが伸びていました。
`BTree::lookup`が本当に`O(log n)`で伸びないかを、同じ`n`(1,000から16,000)で測って確かめます。

```rust
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
```

```console
$ cargo test --release --lib lookup_time_grows_much_slower -- --ignored --nocapture
n=  1000 height=2 elapsed=23.85µs
n=  2000 height=2 elapsed=22.96µs
n=  4000 height=2 elapsed=19.03µs
n=  8000 height=2 elapsed=25.15µs
n= 16000 height=2 elapsed=43.7µs
```

第20章の実測は、`n`を1,000から16,000へ16倍にすると`INSERT`1件あたりのコストもほぼ16倍(29.25µs→349.193µs)に伸びていました。
この章の`lookup`は、同じ16倍の`n`に対して`height`が2のまま変わらず、実行時間も20µsから40µs台のまま目立った伸びを見せません。
`n`を16倍にしても`height`が1つも増えないのは、この章のLeaf Pageが数百件のエントリを1ページに収められるだけの`fanout`を持っているからで、`height`が2から3へ増えるような`n`の桁になって初めて、`lookup`のコストにもう1段ぶんの差が現れます。
それでも「1段増えるごとに定数時間が足されるだけ」という`O(log n)`の性質そのものは、`n`が伸びるほど`O(n)`の第20章の実測との差を指数的に広げ続けます。
この測定は環境に依存する実行時間そのものを検証する回帰テストにはしていません(`#[ignore]`を付けてあります)が、`cargo test --release -- --ignored --nocapture`で読者自身の環境でも再現できます。

第24章でこの索引を`crate::constraints::check_uniqueness`の一意性検査に結線すると、第20章で見た比例関係そのものが解消されます。

## 演習問題

### 必須課題

1. `BTree::insert`に、すでに`lookup`で確認できる件数のキーを挿入した後、さらに`BTree::create`時とは異なる`DataType`の`Value`を`insert`したときに`DbError::BTreeKeyTypeMismatch`が返ることを、`expected`と`actual`フィールドの中身まで検証するテストを追加してください。
2. `LeafPageRef::find`と`InternalPageRef::child_for`は、どちらも`lo`と`hi`だけを持つ手書きの二分探索です。この二分探索を、添字配列を`Vec`へ`collect`してから`slice::binary_search_by`(または`partition_point`)に渡す実装に書き換え、`many_keys_force_multi_level_split_and_all_remain_findable`のような多段Splitを伴うテストの実行時間が、変更前後でどれだけ変わるかを`std::time::Instant`で比較してください。
3. `split_leaf`と`split_internal`は、エントリをちょうど半分(`entries.len() / 2`)で分割します。この分割位置を「前半3分の1、後半3分の2」のように変えると、`many_keys_force_multi_level_split_and_all_remain_findable`が確認している木の高さや、`insert`を連続して呼んだときのSplit発生回数がどう変わるかを実測してください。

### 発展課題

1. `crate::constraints::check_uniqueness`(第20章)は`PRIMARY KEY`や`UNIQUE`列に対して線形走査で一意性を検査していました。この章の`BTree`を使い、挿入しようとしている値がすでに索引にあるかどうかを`lookup`で確認する`check_uniqueness_with_index`という関数を設計してください(実装まで求めるものではありません)。挿入候補が複数行ある`INSERT`文の中で互いに重複している場合(第20章の「候補同士の比較」)を、この索引だけで検出できるか、できないとすればどんな補助データが要るかを検討してください。
2. この章の`insert`は、Splitのたびに親のInternal Pageを`read_page`で1回、`write_page`で1回、計2回開いています。Leaf SplitからRoot Splitまで一直線に伝播する最悪ケースでは、木の高さに比例した回数のページI/Oが発生します。この回数を`BufferPool::stats()`(第14章の`BufferPoolStats`)を使って実際に数え、ヒット率がどれくらいになるかを`many_keys_force_multi_level_split_and_all_remain_findable`相当の挿入で計測してください。
3. `split_leaf`と`split_internal`はどちらも、収まらなかった場合に`DbError::BTreeKeyTooLarge`を返すだけで、それ以上の対応(3分割、キーの圧縮など)を行いません。`TEXT`キーの先頭バイト列だけを索引に持たせ、完全な値との一致は該当する`RecordId`のタプルを実際に読んで確認する「プレフィックス圧縮」を導入すると、この上限がどれだけ緩和できるかを設計してください。
