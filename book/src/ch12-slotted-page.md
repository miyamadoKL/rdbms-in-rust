# 第12章 Slotted Page、Tuple、RID

前章で`Page`は、`PAGE_SIZE`(4096バイト)のうち先頭16バイトをPage Headerに、残り`PAGE_PAYLOAD_SIZE`(4080バイト)を`payload`に割り当てるところまで決まりました。
試しに、この4080バイトへの可変参照を返す`Page::payload_mut()`を使って、次のように書き込んでみます。

```rust
let mut page = Page::new(PageId(1), PageType::Data);
page.payload_mut()[0..5].copy_from_slice(b"hello");
```

このコードは実際にコンパイルが通り、動きます。
けれども、これは`payload`の使い方としては場当たり的です。
2件目のタプルをどこから書き始めればよいのか、`payload`は何も教えてくれません。
1件目が何バイトだったかを覚えているのは、このコードを書いた人間の記憶だけです。

`users`テーブルに`(1, 'Alice')`と`(2, 'Bob')`を`INSERT`したとしましょう。
`id`は`BIGINT`なので8バイト固定ですが、`name`は`TEXT`なので`'Alice'`と`'Bob'`はバイト数が異なります。
2件目を1件目のすぐ後ろに詰めて書けば無駄なく並びますが、そのすぐ後で`DELETE FROM users WHERE id = 1`を実行するとどうなるでしょうか。
1件目の場所には隙間が空き、2件目のバイト列だけが取り残されます。
その隙間を埋めるには、2件目を前へずらして書き直すしかありません。
これは、第11章で「ファイル全体を1本のバイト列として素朴に扱う案」を退けた理由と同じ形の問題です。
その問題をファイル全体からページ1枚に縮小しただけで、まだ何も解決していません。

この章では、1枚の`payload`の中に可変長のタプルを詰めるための構造を作ります。
これが**Slotted Page**です。

## なぜ固定長スロットではなく間接参照を使うのか

可変長のタプルを扱う設計として、まず考えられるのは「1タプルあたりの最大バイト数を決めておき、`payload`をその大きさの固定長区画に分割する」という案です。
`BIGINT`と`BOOLEAN`は元々固定長なので、この案でもうまくいきます。

しかし`TEXT`はそうはいきません。
`name TEXT`という列1つに、1文字の`'A'`も1000文字の長い自己紹介文も等しく収まらなければならないとすると、区画の大きさはその最大値に合わせて決めるしかありません。
大半のタプルが短い`TEXT`しか持たないとしても、区画は常に最大値分の`payload`を消費し、実際に使われないバイトのほとんどが無駄になります。
そして、その最大値をいくつにするかという問いに、この章の時点では答えがありません。

そこで、タプルの実データと、それを指し示す場所を分けます。
`payload`の先頭側に、各タプルへの間接参照(オフセットとバイト数の組)を並べた表を置きます。
これを**Slot Directory**と呼びます。
実際のタプルのバイト列は、`payload`の末尾側に、可変長のまま詰めて書き込みます。
外部からタプルを指すときは、実データの位置(いずれ動きうる)ではなく、Slot Directory内の番号(**スロット**)を指します。

この間接参照には、もう1つ効きどころがあります。
`DELETE`や`UPDATE`によってタプルの実データが移動しても、指す先がスロット番号である限り、外部からの参照はそのまま有効です。
テーブルの1行を指す識別子(**RID**、後述)がタプルの物理的な位置に依存しないという性質は、第23章以降でB+Treeのインデックスがこの識別子を値として保持するときに重要になります。
インデックスがタプルの位置そのものを直接指していたら、ページ内でタプルが1バイトでも動くたびに、そのタプルを指す全てのインデックスエントリを書き換えなければならなくなるはずです。

## payload内のレイアウトを設計する

`payload`は3つの領域に分かれます。
Slot Directoryは先頭から後ろへ、Tuple Dataは末尾から前へ向かって、互いに向き合う形で成長します。

```text
0                                                        payload.len()
+----------+----------------+---------------+---------------+
| Header   | Slot Directory |  Free Space   |   Tuple Data   |
| (4バイト)|       →        |               |        ←       |
+----------+----------------+---------------+---------------+
           ^                                ^
           SLOTTED_HEADER_SIZE          tuple_data_start
```

先頭4バイトは、このpayload自身についてのヘッダーです。

| オフセット | バイト数 | フィールド | 内容 |
| --- | --- | --- | --- |
| 0 | 2 | `slot_count` | Slot Directoryのエントリ数(LE) |
| 2 | 2 | `tuple_data_start` | Tuple Data領域の開始位置(LE) |

`slot_count`にはOccupied(有効)なスロットだけでなく、後述するTombstone(削除済み)のスロットも数に含みます。
`tuple_data_start`は、末尾から詰めているTuple Data領域の、現時点で最も先頭に近い位置です。
新しいタプルを1件書き込むたびにこの値は減っていき、`payload`の先頭に近づいていきます。

ヘッダーのすぐ後ろから、Slot Directoryが1エントリ8バイトで並びます。

| オフセット | バイト数 | フィールド | 内容 |
| --- | --- | --- | --- |
| 0 | 2 | `offset` | このスロットが指すTuple Data領域内の開始位置(LE) |
| 2 | 2 | `length` | タプルのバイト数(LE) |
| 4 | 1 | `status` | `1`(Occupied)または`2`(Tombstone) |
| 5 | 3 | (予約領域) | 常に0。将来のスロット拡張用 |

`slot_count`番目のスロットは、`payload`のオフセット`SLOTTED_HEADER_SIZE + slot_count * SLOT_ENTRY_SIZE`から8バイトに書かれています。
スロット番号さえ分かれば、この掛け算1つでSlot Directory内の位置を直接求められ、先頭から順に走査する必要はありません。

ここから先のコードは、この章で新しく作る`src/slotted_page.rs`に置いていきます。
まず、この2つの定数を定義します。

```rust
pub const SLOTTED_HEADER_SIZE: usize = 4;
pub const SLOT_ENTRY_SIZE: usize = 8;
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod slotted_page;
```

Slot DirectoryとTuple Dataは、互いに向かい合って伸びる分だけ、いずれ衝突する可能性があります。
この章のコードには、次の不変条件を常に保つという約束があります。

**不変条件**: 常に`SLOTTED_HEADER_SIZE + slot_count * SLOT_ENTRY_SIZE <= tuple_data_start`が成り立つ。

この式の左辺はSlot Directoryが埋めているバイト数、右辺の`tuple_data_start`はTuple Data領域の開始位置です。
左辺が右辺を超えることは、Slot DirectoryとTuple Dataの一部が同じバイトを取り合っていることを意味します。
`init`は空のSlotted Pageとしてこの条件を満たす状態から始め、`insert`と`update`はいずれも書き込みを行う前にこの条件を保てるかどうかを確認し、保てない場合は書き込みを行わずに失敗を返します。
`compact`は既存のスロットを動かして詰め直すだけで新たに確保しないため、そもそも失敗しうる操作ではありません。
この3つの操作だけを経由してきた`payload`であれば、不変条件は常に保たれます。

## Record ID: ページの中の1行をどう指すか

Slot Directoryの1エントリを指す番号を`SlotId`という型で表します。
`PageId`と同じくNewtypeで、`u64`ではなく`u16`を包みます。
4080バイトの`payload`に収まるスロット数は最大でも510件程度なので、`u16`(最大65535)で十分足ります。
`SlotId`は、既存の`src/ids.rs`に追記します。

```rust
pub struct SlotId(pub u16);
```

`SlotId`だけでは、データベース全体でタプル1件を特定できません。
あるページのスロット3と、別のページのスロット3は無関係な区画です。
`PageId`と`SlotId`の組があって初めて、データベース全体で1件のタプルの位置を指せます。
この組を**Record ID**(**RID**)と呼び、同じ`src/ids.rs`に`RecordId`という構造体を追記して表します。

```rust
pub struct RecordId {
    pub page_id: PageId,
    pub slot_id: SlotId,
}
```

`RecordId`が「タプルを指す識別子」として意味を持ち続けるためには、あるタプルの`RecordId`が指す先が、そのタプルが存在する限り変わらないことが望ましいところです。
Slot Directoryという間接参照は、この安定性のためにあります。
`compact`(後述)によってタプルの実データが`payload`内の別の位置へ移動しても、書き換わるのはスロットが指す`offset`だけであり、スロット番号自体、つまり`RecordId`は変わりません。
この安定性が保証されるのは、あくまで1枚のページの中で完結する操作(この章の`insert`、`update`、`compact`)に限られます。
タプルが元のページに収まらなくなり、別のページへ移し替えられる場合には話が別です。
第13章の`HeapFile::update`は、ページをまたぐ移動が起きたときに`RecordId`を変えるという設計を選びます。

## タプルを挿入する

`src/slotted_page.rs`に次の`SlottedPage`を定義します。

```rust
pub struct SlottedPage<'a> {
    payload: &'a mut [u8],
}
```

`SlottedPage`は`Page`の`payload`を借用するビューです。
新しく作った`payload`には`SlottedPage::init`を、すでにSlotted Pageとして書き込み済みの`payload`には`SlottedPage::open`を使います。
`open`は`DbResult<Self>`を返します。
`init`から始まり、この章の操作だけを経由してきた`payload`であれば不変条件は保たれているはずですが、`open`はその前提を無条件には信用しません。
検証は4段階です。

1. `payload`自体が最低でもヘッダー(`SLOTTED_HEADER_SIZE`バイト)を持つこと。これより短いと、ヘッダーを読む処理自体がスライス添字でpanicします。
2. ヘッダーの`slot_count`と`tuple_data_start`がこの`payload`の大きさに対して妥当な範囲(不変条件`SLOTTED_HEADER_SIZE + slot_count * SLOT_ENTRY_SIZE <= tuple_data_start <= payload.len()`)に収まっていること。
3. 各スロットの`status`が`1`(Occupied)か`2`(Tombstone)のどちらかであること。それ以外の値は、`status`や`get`の内部の`match`をpanicさせる原因になります。
4. `status`が`Occupied`のスロットについて、`offset`と`length`が指す範囲がTuple Data領域(`[tuple_data_start, payload.len())`)に収まっていること、かつOccupiedなスロット同士のタプルデータ範囲が互いに重なっていないこと。

いずれかを満たさなければ`DbError::CorruptPage`を返します。
検証を怠ると、破損した(あるいは`Data`以外の`PageType`の)バイト列を`payload`として渡されたとき、以後のスライス添字アクセスがそのままpanicしてしまいます。
`Tombstone`のスロットの`offset`と`length`は4の検証対象に含めません。
`compact`(後述)は生きているスロットだけを詰め直す際、`Tombstone`の`offset`と`length`をそのまま残す(値を書き換えない)ため、`tuple_data_start`より手前を指していても壊れているとは言えず、`get`と`compact`のどちらからも参照されないからです。

4の「重なっていないこと」の判定には、長さ0のタプル(空の`&[]`を`insert`した場合に実際に起こりえます)が絡む落とし穴があります。
長さ0のタプルの範囲は`start == end`の空区間で、その定義上どのバイトも占有しないため、他のどんな範囲とも交差しえません。
ところが、Occupiedな範囲を`start`だけをキーにソートしてから隣接する範囲だけを比べるという実装をそのまま使うと、空区間`[4079, 4079)`と、たまたま同じ`start`を持つ非空区間`[4079, 4080)`が並んだときに、ソートの並び順(どちらが先に来るかは`start`だけでは決まりません)次第で「重なっている」と誤判定することがあります。
この検証は、空区間をそもそも重複検査の対象から除外することでこの落とし穴を避けています。
読み取り専用の`SlottedPageRef::open`(第14章で登場)も同じ検証を行います。

`init`は`src/slotted_page.rs`に定義します。

```rust
pub fn init(payload: &'a mut [u8]) -> Self {
    let capacity = payload.len();
    assert!(
        capacity <= u16::MAX as usize,
        "payloadがu16に収まらないほど大きいです: {capacity}バイト"
    );
    let mut page = SlottedPage { payload };
    page.set_header(0, capacity as u16);
    page
}
```

`init`は`slot_count`を0、`tuple_data_start`を`payload`全体の長さに設定します。
Slot Directoryが1件も無く、Tuple Data領域がまだ1バイトも使われていない、空のSlotted Pageです。

同じ`src/slotted_page.rs`に追加する`insert`は、バイト列を1件のタプルとして書き込み、そのタプルを指す`SlotId`を返します。

```rust
pub fn insert(&mut self, bytes: &[u8]) -> Option<SlotId> {
    if let Some(slot) = self.try_insert(bytes) {
        return Some(slot);
    }
    self.compact();
    self.try_insert(bytes)
}
```

実際の割り当ては、同じ`src/slotted_page.rs`の`try_insert`が行います。

```rust
fn try_insert(&mut self, bytes: &[u8]) -> Option<SlotId> {
    let len = bytes.len();
    let (slot_count, tuple_data_start) = self.header();

    let reuse = (0..slot_count)
        .map(SlotId)
        .find(|&s| matches!(self.slot_entry(s), Some((_, _, STATUS_TOMBSTONE))));

    let grows_directory = reuse.is_none();
    let directory_end = self.directory_end(slot_count + u16::from(grows_directory));
    if directory_end + len > tuple_data_start as usize {
        return None;
    }

    let new_start = tuple_data_start as usize - len;
    self.payload[new_start..new_start + len].copy_from_slice(bytes);

    let slot = reuse.unwrap_or(SlotId(slot_count));
    self.set_slot_entry(slot, new_start as u16, len as u16, STATUS_OCCUPIED);
    let new_slot_count = slot_count + u16::from(grows_directory);
    self.set_header(new_slot_count, new_start as u16);
    Some(slot)
}
```

最初に、既存のスロットの中にTombstone(削除済み、後述)のものが無いかを探します。
見つかれば、Slot Directoryを増やさずそのスロット番号をそのまま使い回します。
見つからなければ、Directoryの末尾に新しいスロットを1つ追加します。
どちらの場合でも、書き込み先はDirectoryの終端(`directory_end`)から`tuple_data_start`までの空き領域です。
この範囲に新しいタプルの長さが収まるかどうかを確認してから、収まる場合だけ`tuple_data_start`を新しい書き込み位置まで縮め、そのタプルの`offset`と`length`をスロットへ記録します。

空き領域が足りず`try_insert`が`None`を返した場合、`insert`はすぐには諦めません。
1度`compact`を呼んでから、もう1度`try_insert`を試します。
`payload`の中に削除済みタプルの死んだバイト列が残っている場合、それを回収すれば新しいタプルが入る余地が生まれることがあるためです。
コンパクションの中身は後の節で扱い、ここでは「挿入が失敗しそうなときに、まず回収を試みてから改めて判定する」という`insert`の外側の振る舞いだけを押さえておきます。

コンパクションを行ってもなお空き領域が足りない場合、`insert`は`None`を返します。
このページには、物理的にそのタプルを置く場所がありません。
呼び出し側(第13章のHeap File)は、この`None`を「このページはもう使えない、別のページを探す」という合図として扱うことになります。

呼び出し側にとって、この`None`が返ってくるのがいつも都合のよいタイミングとは限りません。
`bytes`自体が、空の1ページにすら収まらないほど大きい場合を考えます。
第13章のHeap Fileは、既存のページで`None`を受け取るたびに次のページを試し、最後に新しいページを1枚割り当ててからようやくその`bytes`が入らないと分かります。
この時点で、失敗するだけの`insert`のためにページを1枚確保してしまっています。
そこで、実際にページへ触れる前に「そもそも入りうるかどうか」を計算だけで判定できる関数を`src/slotted_page.rs`に用意しておきます。

```rust
pub fn max_len_for_fresh_page(payload_len: usize) -> usize {
    payload_len.saturating_sub(SLOTTED_HEADER_SIZE + SLOT_ENTRY_SIZE)
}
```

これは、スロット0件の(空の)`payload`へ新しいスロットを1つ追加する際の`try_insert`の判定(`directory_end(1) + len > tuple_data_start`、空の`payload`では`tuple_data_start == payload_len`)を、`payload`にいっさい触れずに計算し直したものです。
第13章以降のHeap FileとStorageの`insert`と`update`は、実際にページを確保して初期化する前にこの関数で`bytes.len()`を検査し、収まりえないと分かればその場で`DbError::TupleTooLarge`を返します。

## 削除はTombstoneにとどめる

同じ`src/slotted_page.rs`に追加する`delete`は、指定したスロットを削除済みとして印付けます。

```rust
pub fn delete(&mut self, slot: SlotId) -> bool {
    match self.slot_entry(slot) {
        Some((offset, length, STATUS_OCCUPIED)) => {
            self.set_slot_entry(slot, offset, length, STATUS_TOMBSTONE);
            true
        }
        _ => false,
    }
}
```

`delete`が書き換えるのは`status`だけです。
`offset`と`length`はそのまま残り、Tuple Data領域の実データも1バイトも書き換わりません。
このタプルが占めていた`payload`上のバイト列は、その場に置き去りにされたまま、以後どのスロットからも参照されない**死んだ領域**になります。

このタプルの状態を、Slot Directoryを直接削除するのではなく`Tombstone`という中間状態にとどめておく理由は2つあります。
1つは、`RecordId`の安定性です。
Slot Directoryのエントリを本当に削除して後続のスロットを詰めてしまうと、それらのスロット番号がずれ、そのスロットを指していた`RecordId`が別のタプルを指すようになってしまいます。
`Tombstone`はその場に留まり続けるので、この問題は起きません。
もう1つは、`src/slotted_page.rs`に追加する`get`が、この状態を見分けられることです。

```rust
pub fn get(&self, slot: SlotId) -> Option<&[u8]> {
    let (offset, length, status) = self.slot_entry(slot)?;
    if status != STATUS_OCCUPIED {
        return None;
    }
    let start = offset as usize;
    Some(&self.payload[start..start + length as usize])
}
```

`get`は`status`が`Occupied`のときだけタプルのバイト列を返します。
`Tombstone`のスロットに対しては、スロット自体は存在するにもかかわらず`None`を返します。
「そのRIDはかつて存在したが、今は存在しない」ことと、「そのRIDは一度も存在したことがない」ことは、`SlottedPage`の内部では別の状態として区別されていますが、`get`を呼ぶ側からはどちらも同じ`None`として見えます。
外部の参照(第23章以降のインデックスが持つ`RecordId`)から見れば、この2つは「今読めない」という点で等価であり、区別する必要が無いためです。

`Tombstone`になったスロットは、`try_insert`が最初に探す再利用先でもあります。
削除済みのRecord IDをそのまま次のタプルに使い回すことで、Slot Directory自体を無限に伸ばさずに済みます。

## 断片化を解消するコンパクション

削除や、後述する`update`によるサイズ変更が繰り返されると、Tuple Data領域の中に死んだバイト列が散らばっていきます。
`payload`全体としての空きバイト数は十分でも、それが1箇所にまとまっていなければ、新しい大きめのタプルを書き込む連続した領域を確保できません。
この状態を**断片化**と呼びます。

同じ`src/slotted_page.rs`に追加する`compact`は、生きている(Occupiedな)タプルだけを集めてTuple Data領域を隙間なく詰め直し、断片化を解消します。

```rust
pub fn compact(&mut self) {
    let (slot_count, _) = self.header();

    let mut live: Vec<(SlotId, Vec<u8>)> = Vec::new();
    for i in 0..slot_count {
        let slot = SlotId(i);
        if let Some((offset, length, STATUS_OCCUPIED)) = self.slot_entry(slot) {
            let start = offset as usize;
            live.push((slot, self.payload[start..start + length as usize].to_vec()));
        }
    }

    let mut cursor = self.payload.len();
    for (slot, bytes) in &live {
        cursor -= bytes.len();
        self.payload[cursor..cursor + bytes.len()].copy_from_slice(bytes);
        self.set_slot_entry(*slot, cursor as u16, bytes.len() as u16, STATUS_OCCUPIED);
    }
    self.set_header(slot_count, cursor as u16);
}
```

まず、全スロットを走査してOccupiedなものだけのバイト列を一時的な`Vec`へ退避します。
`payload`の同じ領域を読みながら書き換えると、後から処理するタプルのバイト列を上書きしてしまう恐れがあるため、書き戻す前に一旦コピーを取っておく必要があります。
退避が終わったら、`payload`の末尾から詰め直す形で、生きているタプルだけを連続した領域に書き戻します。
このとき各タプルの`offset`は新しい位置に更新されますが、スロット番号自体もタプルの中身も変わりません。

Slot Directoryそのもの(`slot_count`や、Tombstoneのままのエントリ)は`compact`の対象ではありません。
末尾に近いTombstoneを削って`slot_count`を縮める最適化も考えられますが、この章では実装していません。
`slot_count`を縮めてよいのは末尾に連続したTombstoneがある場合に限られ、途中に1つでもOccupiedなスロットが挟まっていれば縮められないという条件分岐が増えるわりに、この章の時点ではSlot Directory自体のバイト数(タプル本体に比べて小さい)を削る効果は限定的です。
回収すべき対象は主にTuple Data領域の死んだバイト列であり、`compact`はそこに絞っています。

## サイズが変わる更新

同じ`src/slotted_page.rs`に追加する`update`は、指定したスロットが指すタプルを新しいバイト列に置き換えます。

```rust
pub fn update(&mut self, slot: SlotId, bytes: &[u8]) -> bool {
    let Some((offset, length, status)) = self.slot_entry(slot) else {
        return false;
    };
    if status != STATUS_OCCUPIED {
        return false;
    }

    if bytes.len() == length as usize {
        let start = offset as usize;
        self.payload[start..start + bytes.len()].copy_from_slice(bytes);
        return true;
    }

    if self.try_relocate(slot, bytes) {
        return true;
    }
    self.compact();
    self.try_relocate(slot, bytes)
}
```

新しいバイト列の長さが元と同じ場合、話は単純です。
Tuple Data領域の同じ位置へ、新しいバイト列をそのまま上書きするだけで済み、Slot Directoryはいっさい変更しません。

長さが変わる場合はそうはいきません。
`BIGINT`や`BOOLEAN`は値が変わってもバイト数は変わりませんが、`TEXT`は`UPDATE users SET name = 'Alexandria' WHERE id = 1`のように、元の`'Alice'`より長い文字列に置き換わることがあります。
その場合、元の場所にそのまま収まる保証はありません。
そこで、同じ`src/slotted_page.rs`に`try_relocate`という関数を追加します。

```rust
fn try_relocate(&mut self, slot: SlotId, bytes: &[u8]) -> bool {
    let len = bytes.len();
    let (slot_count, tuple_data_start) = self.header();
    let directory_end = self.directory_end(slot_count);
    if directory_end + len > tuple_data_start as usize {
        return false;
    }
    let new_start = tuple_data_start as usize - len;
    self.payload[new_start..new_start + len].copy_from_slice(bytes);
    self.set_slot_entry(slot, new_start as u16, len as u16, STATUS_OCCUPIED);
    self.set_header(slot_count, new_start as u16);
    true
}
```

`try_relocate`は、`insert`が新しいタプルを書き込むのとほぼ同じ手順で、新しいバイト列をTuple Data領域の空いている側に書き込み、対象スロットの`offset`と`length`だけを新しい位置へ付け替えます。
元の位置にあった古いバイト列は、そのまま死んだ領域として残ります。
これは`delete`が古いバイト列を消さないのと同じ考え方で、いずれ`compact`が回収します。
`try_relocate`が空き領域不足で失敗した場合、`update`は`insert`と同様に1度`compact`してから再試行し、それでも入らなければ`false`を返して対象のタプルを元のまま残します。

サイズが縮む場合(たとえば長い`TEXT`を短い`TEXT`へ更新する場合)も、同じ`try_relocate`を通ります。
元の場所より小さいバイト列を書くために、あえて新しい位置へ移すのは無駄に見えるかもしれません。
しかし、元の位置に新しい(より短い)バイト列だけを書いて残りを空けておくと、その空いた分は他のどのスロットからも参照されない中途半端な隙間になり、結局`compact`でしか回収できません。
新しい位置への書き込みも、古い位置の残骸も、どちらも回収は`compact`任せという点は変わらないため、この章では縮む場合も伸びる場合も同じ`try_relocate`1本にまとめ、経路を1つに保っています。

`update`が返す`bool`のうち、対象のスロットが存在しない場合と`Tombstone`(削除済み)の場合はどちらも`false`です。
すでに存在しないタプルを更新しようとする呼び出しは、この章のAPIレベルでは「対象が見つからなかった」という1種類の失敗として扱われます。

## TupleをバイトへEncode/Decodeする

ここまでの`SlottedPage`は`&[u8]`しか扱いません。
`INSERT INTO users VALUES (1, 'Alice')`の`(1, 'Alice')`は、第4章の`Tuple`型では`Value::BigInt(1)`と`Value::Text("Alice")`の並びですが、これを`SlottedPage::insert`に渡すには、あらかじめバイト列へ変換しておく必要があります。
この変換を担うのが`tuple_codec`モジュールです。

バイト列は、NULLビットマップと、NULLでない列の値を並べたものです。

```text
+----------------+----------+----------+-----+----------+
| NULLビットマップ | 列0の値  | 列1の値  | ... | 列N-1の値 |
+----------------+----------+----------+-----+----------+
```

NULLビットマップは、列数を8列単位へ切り上げたバイト数を持ちます。
列`i`が`NULL`なら、`i / 8`バイト目の`i % 8`ビット目が1になります。

ここから先のコードは、この章で新しく作る`src/tuple_codec.rs`に置いていきます。
まず、この関数を定義します。

```rust
fn null_bitmap_len(column_count: usize) -> usize {
    column_count.div_ceil(8)
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod tuple_codec;
```

`NULL`は`Value::Null`という1種類の値であり、`DataType`を持ちません。
どの列が`NULL`かはビットマップだけで判定でき、値の領域には`NULL`のための表現を一切書き込む必要がありません。
`NULL`でない列は、ビットが立っていないことが分かった上で、`Schema`が定めるその列の`DataType`に従ってデコードします。

| `DataType` | バイト数 | 形式 |
| --- | --- | --- |
| `BOOLEAN` | 1 | `0`(false)または`1`(true) |
| `BIGINT` | 8 | `i64`のリトルエンディアン表現 |
| `TEXT` | `4 + len` | 長さ`len`(`u32`、リトルエンディアン)に続けてUTF-8バイト列 |

`BOOLEAN`と`BIGINT`は固定長なので、値の種類が分かればバイト数も一意に決まります。
`TEXT`だけが可変長であり、長さを先に書いておかなければ、どこまでがその列の値でどこからが次の列の値かを区別できません。
この**長さプレフィックス**方式のおかげで、`decode_tuple`は区切り文字を探す必要も、文字列の終端記号を予約する必要もなく、読むべきバイト数を先頭4バイトから直接計算できます。

同じ`src/tuple_codec.rs`に定義する`encode_tuple`は、この形式に従って`Tuple`をバイト列へ変換します。

```rust
pub fn encode_tuple(schema: &Schema, tuple: &Tuple) -> Vec<u8> {
    let values = tuple.values();
    debug_assert_eq!(
        values.len(),
        schema.len(),
        "tupleの列数がschemaと一致しません"
    );
    let mut bitmap = vec![0u8; null_bitmap_len(values.len())];
    let mut body = Vec::new();

    for (i, value) in values.iter().enumerate() {
        match value {
            Value::Null => {
                bitmap[i / 8] |= 1 << (i % 8);
            }
            Value::Boolean(b) => {
                body.push(u8::from(*b));
            }
            Value::BigInt(n) => {
                body.extend_from_slice(&n.to_le_bytes());
            }
            Value::Text(s) => {
                let bytes = s.as_bytes();
                body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }

    let mut out = bitmap;
    out.extend_from_slice(&body);
    out
}
```

`Value`自身が`Null`、`Boolean`、`BigInt`、`Text`のどれであるかを持っているため、`encode_tuple`はどの列がどの`DataType`かを`schema`から調べ直す必要がありません。
それでも`schema`を引数に取っているのは、渡された`tuple`の列数が呼び出し側の想定する`schema`と食い違っていないかを`debug_assert_eq!`で確かめるためです。

`decode_tuple`は、この逆方向の変換を行います。
`Value`自身に型情報を持たない生のバイト列からは、各列が何列目のどの`DataType`かを`schema`を見て判断するしかありません。
同じ`src/tuple_codec.rs`に、次の`decode_tuple`を定義します。

```rust
pub fn decode_tuple(schema: &Schema, bytes: &[u8]) -> DbResult<Tuple> {
    let column_count = schema.len();
    let bitmap_len = null_bitmap_len(column_count);
    if bytes.len() < bitmap_len {
        return Err(DbError::CorruptTuple(format!(
            "NULLビットマップのバイト数が不足しています: {bitmap_len}バイトが必要ですが{}バイトしかありません",
            bytes.len()
        )));
    }
    let bitmap = &bytes[0..bitmap_len];
    let mut cursor = bitmap_len;

    let mut values = Vec::with_capacity(column_count);
    for (i, column) in schema.columns().iter().enumerate() {
        let is_null = bitmap[i / 8] & (1 << (i % 8)) != 0;
        if is_null {
            values.push(Value::Null);
            continue;
        }

        let value = match column.data_type {
            DataType::Boolean => {
                let byte = *bytes.get(cursor).ok_or_else(|| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(BOOLEAN)を読む前にバイト列が尽きました",
                        column.name
                    ))
                })?;
                cursor += 1;
                Value::Boolean(byte != 0)
            }
            DataType::BigInt => {
                let end = cursor + 8;
                let slice = bytes.get(cursor..end).ok_or_else(|| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(BIGINT)の8バイトを読む前にバイト列が尽きました",
                        column.name
                    ))
                })?;
                cursor = end;
                Value::BigInt(i64::from_le_bytes(slice.try_into().unwrap()))
            }
            DataType::Text => {
                let len_end = cursor + 4;
                let len_bytes = bytes.get(cursor..len_end).ok_or_else(|| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(TEXT)の長さプレフィックスを読む前にバイト列が尽きました",
                        column.name
                    ))
                })?;
                let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
                cursor = len_end;
                let text_end = cursor + len;
                let text_bytes = bytes.get(cursor..text_end).ok_or_else(|| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(TEXT)の本体を{len}バイト読む前にバイト列が尽きました",
                        column.name
                    ))
                })?;
                cursor = text_end;
                let text = String::from_utf8(text_bytes.to_vec()).map_err(|_| {
                    DbError::CorruptTuple(format!(
                        "列'{}'(TEXT)の内容が妥当なUTF-8ではありません",
                        column.name
                    ))
                })?;
                Value::Text(text)
            }
        };
        values.push(value);
    }

    Tuple::new(schema, values)
}
```

`schema.columns()`を先頭から順に見ていき、ビットマップでNULLと分かった列は値を読まずに`Value::Null`を積み、そうでない列は`column.data_type`に従ってバイト列を読み進めます。
`BOOLEAN`なら1バイト、`BIGINT`なら8バイト、`TEXT`なら4バイトの長さプレフィックスとその分の本体、というように、列ごとに読むべきバイト数が変わるため、`cursor`という変数でバイト列内の現在位置を追いながら1列ずつ前進させています。
全列を読み終えたら、最後に`Tuple::new`へ渡します。
`Tuple::new`自身が`Schema`との適合を検査する第4章の関数なので、`decode_tuple`が組み立てた値の並びが本当に`schema`へ適合しているかどうかは、ここでもう一度確認されます。

`bytes`が短すぎる場合や、`TEXT`の長さプレフィックスが実際の残りバイト数を超えている場合、`decode_tuple`は`DbError::CorruptTuple`を返します。
`bytes.get(range)`のように範囲外アクセスを`Option`として受け取る形で境界チェックを行っているため、不正な`bytes`を渡してもパニックせず、この章で追加するエラーとして呼び出し側に伝わります。

この`CorruptTuple`は、既存の`src/error.rs`の`DbError`に追記します。

```rust
/// Tupleのバイト列が、渡された`Schema`のもとで復元できないエラー
/// (バイト列がNULLビットマップや値の途中で尽きている、`TEXT`の長さプレフィックス
/// が実際の残りバイト数を超えているなど)。
#[error("破損したタプルです: {0}")]
CorruptTuple(String),
```

## テストで確認する

`slotted_page`と`tuple_codec`のテストは、大きく5つの観点をカバーしています。

挿入してすぐ`get`すれば同じバイト列が返ることを確認する、最も基本的なラウンドトリップです。
これらのテストは`src/slotted_page.rs`の`#[cfg(test)] mod tests`に置きます。

```rust
#[test]
fn insert_then_get_round_trips() {
    let mut payload = fresh_payload();
    let mut page = SlottedPage::init(&mut payload);

    let slot = page.insert(b"hello").unwrap();
    assert_eq!(page.get(slot), Some(&b"hello"[..]));
    assert_eq!(page.slot_count(), 1);
}
```

同じ`src/slotted_page.rs`のテストで、`payload`をほぼ埋め尽くす1件を先に入れておき、その後の挿入が空き領域不足で`None`を返すことも確認しています。

```rust
#[test]
fn insert_returns_none_when_page_is_full() {
    let mut payload = fresh_payload();
    let mut page = SlottedPage::init(&mut payload);

    // ページ本体をほぼ埋め尽くす1件を先に入れておく。
    let big = vec![b'x'; PAGE_PAYLOAD_SIZE - SLOTTED_HEADER_SIZE - SLOT_ENTRY_SIZE - 4];
    let slot = page.insert(&big).unwrap();
    assert_eq!(page.get(slot), Some(big.as_slice()));

    // 残りの空きより大きいタプルは入らない。
    assert_eq!(page.insert(b"12345678"), None);
    // 既存のタプルは無事なまま。
    assert_eq!(page.get(slot), Some(big.as_slice()));
}
```

失敗した挿入が既存のタプルを壊していないことも、同じテストの最後の1行で確かめています。

同じ`src/slotted_page.rs`のテストで、`delete`してからの再挿入が、同じ`SlotId`を使い回すことも確認します。

```rust
#[test]
fn insert_reuses_a_tombstoned_slot_id() {
    let mut payload = fresh_payload();
    let mut page = SlottedPage::init(&mut payload);

    let s1 = page.insert(b"first").unwrap();
    let _s2 = page.insert(b"second").unwrap();
    page.delete(s1);
    assert_eq!(page.slot_count(), 2);

    let s3 = page.insert(b"third").unwrap();
    // Directoryを増やさず、Tombstone化済みのs1をそのまま使い回す。
    assert_eq!(s3, s1);
    assert_eq!(page.slot_count(), 2);
    assert_eq!(page.get(s3), Some(&b"third"[..]));
}
```

`src/slotted_page.rs`に次の`SlotStatus`を定義します。

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotStatus {
    Occupied,
    Tombstone,
}
```

`compact`の前後でタプルの中身が変わらないことは、その等価性そのものがコンパクションの正しさの定義なので、同じ`src/slotted_page.rs`で直接テストします。

```rust
#[test]
fn compact_preserves_tuple_contents_by_slot_id() {
    let mut payload = fresh_payload();
    let mut page = SlottedPage::init(&mut payload);

    let s1 = page.insert(b"alice").unwrap();
    let s2 = page.insert(b"bob").unwrap();
    let s3 = page.insert(b"carol").unwrap();
    page.delete(s2);

    let free_before = page.free_space();
    page.compact();
    let free_after = page.free_space();

    // 死んだbobの領域が回収され、空き領域が増えている。
    assert!(free_after > free_before);
    // 生きているタプルの中身はスロットIDを介して変わらず読める。
    assert_eq!(page.get(s1), Some(&b"alice"[..]));
    assert_eq!(page.get(s3), Some(&b"carol"[..]));
    assert_eq!(page.get(s2), None);
    assert_eq!(page.status(s2), Some(SlotStatus::Tombstone));
}
```

空き領域が増えたことと、生きているタプルが元のスロット番号のまま読めることの両方を、1つのテストで確認しています。

最後に、`src/slotted_page.rs`に、決定的な乱数で挿入と削除をランダムな順序で繰り返し、常に整合性が保たれることを確認するテストを用意しています。

```rust
#[test]
fn random_insert_delete_keeps_slotted_page_consistent() {
    let mut payload = fresh_payload();
    let mut page = SlottedPage::init(&mut payload);
    let mut rng = Xorshift64(0x1234_5678_9abc_def1);

    // (slot, 挿入したバイト列)のうち、まだ削除していないものを追跡する。
    let mut alive: Vec<(SlotId, Vec<u8>)> = Vec::new();

    for i in 0..500 {
        let insert_bias = alive.len() < 4; // 空になりすぎて操作が偏らないようにする。
        let do_insert = insert_bias || rng.range(3) != 0;

        if do_insert {
            let len = 1 + rng.range(64);
            let byte = (i % 251) as u8;
            let bytes: Vec<u8> = vec![byte; len];
            if let Some(slot) = page.insert(&bytes) {
                alive.retain(|(s, _)| *s != slot);
                alive.push((slot, bytes));
            }
            // 挿入が失敗する(ページが本当に満杯)のは許容し、以降の操作を続ける。
        } else {
            let idx = rng.range(alive.len());
            let (slot, _) = alive.remove(idx);
            assert!(page.delete(slot));
        }

        // 生きていると思っているスロットは、常にその通りのバイト列を返す。
        for (slot, bytes) in &alive {
            assert_eq!(page.get(*slot), Some(bytes.as_slice()));
        }
    }

    // 最後にコンパクションしても、生きているタプルの中身は変わらない。
    page.compact();
    for (slot, bytes) in &alive {
        assert_eq!(page.get(*slot), Some(bytes.as_slice()));
    }
}
```

`proptest`のようなランダムテスト専用のクレートを新たに依存に加える代わりに、`xorshift64`という数行のアルゴリズムで自前の疑似乱数生成器を書いています。
シードを`0x1234_5678_9abc_def1`という固定値にしているのは、テストが失敗したときに同じ操作列を再現できるようにするためです。
乱数で毎回シードを変えるテストは、ある回だけ失敗する再現困難なバグを見逃しかねません。
`alive`という`Vec`で「今生きているはずのスロットとその中身」を追跡し、挿入と削除を500回繰り返す間、毎回のループの最後で`alive`に記録した全スロットの中身が実際に一致するかを確認しています。
最後に1度`compact`を呼んでも、この一致が崩れないことも同じテストで確かめています。

`tuple_codec`のテストも同様の構成で、`BOOLEAN`、`BIGINT`、`TEXT`の全ての型、複数列にまたがる`NULL`、8列境界をまたぐビットマップ、マルチバイトのUTF-8文字列のラウンドトリップ、そして意図的に切り詰めたり長さプレフィックスを不正な値に書き換えたりしたバイト列が`CorruptTuple`として拒否されることを確認しています。

## 到達点

この章で作った`SlottedPage`と`tuple_codec`は、まだテーブルのSQL実行経路には接続していません。
`INSERT INTO users VALUES (1, 'Alice')`を実行しても、今までどおり第10章の`MemStorage`が使われ、この章のコードは一切呼ばれません。
`Tuple`を`encode_tuple`でバイト列に変え、`SlottedPage::insert`でページの`payload`に書き込み、`RecordId`でそれを指し示すという一連の流れそのものは、この章のテストの中だけで完結しています。

それでも、次の章以降で必要になる部品はそろいました。
ページ1枚の中に可変長のタプルを詰め、`RecordId`という安定した参照でそれを指し、削除や更新があってもその参照を壊さずに保つ仕組みです。
第13章のHeap Fileは、複数のページにまたがってテーブル全体を扱うために、この`SlottedPage`をページ単位の部品として使います。
`insert`が`None`を返したときに次のページを探すという振る舞いも、Heap Fileがページを使い切ったかどうかを判定する材料になります。

## 演習問題

### 必須課題

1. `SlottedPage::delete`と`SlottedPage::compact`を読み、`delete`が`Tombstone`にした直後の`payload`と、その後`compact`を呼んだ直後の`payload`とで、Tuple Data領域の実際のバイト数(死んだ領域を含めた使用量)がどう変わるかを、`free_space()`の値を実際に出力して確認してください。
2. `try_insert`は、Tombstone化されたスロットを探すときに`(0..slot_count).map(SlotId).find(...)`という形で先頭から順に走査しています。この探索がスロット数に比例した時間を要することを踏まえ、Tombstoneのスロットだけを別に(たとえば`Vec<SlotId>`で)管理して探索を`O(1)`に近づけるとしたら、`delete`、`try_insert`、`compact`のそれぞれをどう変更する必要があるか設計してください(実装は必須ではありません)。
3. `tuple_codec::decode_tuple`の`DataType::Text`の分岐を読み、長さプレフィックスの`u32`をそのまま`usize`へキャストしている箇所を確認してください。32bit環境(`usize`が32bit)でこのキャストが問題を起こしうるかどうか、64bit環境との違いを含めて説明してください。

### 発展課題

1. この章の`SlottedPage`は、削除された領域を`compact`でしか回収しません。ページ全体をコンパクションする代わりに、削除直後のタプルがTuple Data領域の末尾に接している場合に限り即座に`tuple_data_start`を戻す、という部分的な回収を`delete`に追加するとしたらどう実装すべきか考えてください。この最適化がどの程度の場合に効果があり、どの程度の場合には全く効果が無いかも合わせて考察してください。
2. `tuple_codec`の`TEXT`は`u32`の長さプレフィックスを使っていますが、`SlottedPage`が1件のタプルとして扱えるのは`payload`のサイズ(4080バイト程度)が上限です。`u32`ではなく`u16`を長さプレフィックスに使った場合、`tuple_codec`単体のバイト数はどう変わるか、そして`SlottedPage`と組み合わせたときに実害があるかどうかを考えてください。
3. `RecordId`は`PageId`と`SlotId`の組ですが、この章の時点では`RecordId`を実際に保持する場所(インデックスやテーブルの走査結果)がまだ存在しません。第13章のHeap Fileがテーブルを順に走査する`SeqScan`のような操作を実装するとき、返す各行に`RecordId`を添えるべきかどうか、添えるとすれば`UPDATE`や`DELETE`のどんな実装に活用できるかを考えてみてください。
