# 第14章 Buffer Pool

`HeapFile::get`は、`RecordId`を1つ渡されるたびに`disk.read_page(rid.page_id)`を呼び、そのページをディスクから読み直します。
`WHERE id = 42`のような1行だけを引く問い合わせを同じテーブルに何度も投げれば、`get`はそのたびにディスクへ行きます。
`scan`も無関係ではありません。
1回の`scan`はページをまたぐたびに1回だけ`disk.read_page`を呼ぶので、その内部だけを見れば無駄はありませんが、同じテーブルを対象にした`scan`をもう一度呼べば、前回読んだのと同じページをもう一度最初から読み直します。
前章の`insert_across_multiple_pages_and_scan_returns_them_all`テストのように、1回`scan`するだけなら気づきにくい話です。
けれども`SELECT`を伴う問い合わせを繰り返し実行する、あるいはインデックスの無い結合が外側テーブルの行ごとに内側テーブルを丸ごと`scan`し直すような場面では、同じページへの`disk.read_page`が呼び出しの回数だけ積み重なります。
`HeapFile`はページの中身をどこにも留めておかないので、直前に読んだのと同じページであっても、`DiskManager`まで律儀に問い合わせてしまうのです。
このとき`DiskManager::read_page`は何回呼ばれているでしょうか。

## 同じページに触れるたびディスクへ行っている回数を数える

`HeapFile`は、`get`と`scan`のどちらも、必要になったページを`SlottedPage`ごしに読むたびに`disk.read_page(page_id)`を呼びます。
同じページが再び必要になっても、`HeapFile`自身はそのページの中身をどこにも覚えていないので、`DiskManager`はそのたびにもう1度ファイルへ`seek`して`read`し直します。
前章の時点では、これを確かめる手段そのものがありませんでした。

この章ではまず、`src/disk_manager.rs`の`DiskManager`にI/O回数を数える`io_count`を1つ加えます。

```rust
struct Inner {
    file: File,
    /// このファイルが持つページの総数(ページ0のMetaページを含む)。
    /// `FileHeader::page_count`と常に一致する値をメモリ上にも保持しておき、
    /// `read_page`・`allocate_page`のたびにMetaページを読み直さずに済ませる。
    page_count: u64,
    /// `read_page`・`write_page`を呼び出した回数の累計。
    ///
    /// ページの中身には影響しない、純粋な観測用のカウンタである。第14章の
    /// Buffer Poolが、キャッシュを挟まずにこの`DiskManager`へ直接タプル参照の
    /// たびにアクセスすると、この値がアクセス回数に比例して増え続けることを示す。
    io_count: u64,
}
```

`read_page`と`write_page`がこのカウンタを1ずつ増やすだけの変更です。
これを使って、同じページを50回参照するテストを書いてみます。
この章で新規作成する`src/buffer_pool.rs`に、`lib.rs`へ`pub mod buffer_pool;`を追加したうえで、`#[cfg(test)] mod tests`としてこのテストを置きます。

```rust
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
```

同じ1ページを50回読んだだけなのに、`io_count`は50になります。
1ページ目の中身は最初の1回で判明しているのに、`DiskManager`はそれを覚えておらず、毎回律儀にファイルへ`seek`して`read`しています。
これは`DiskManager`の欠陥ではありません。
「`N`番目のページを読む」という要求を、決まった位置への読み書きへ変換するというのが第13章で決めた`DiskManager`の役割そのものであり、その役割にキャッシュは含まれていませんでした。
問題は、その`DiskManager`を`HeapFile`が直接叩いていたことにあります。
`WHERE`句のないテーブル全件走査を10回実行すれば、ページの中身が1バイトも変わっていなくても、ディスクI/Oの回数は10倍になります。

この章で作る**Buffer Pool**は、ページの中身をメモリ上に留めておくことで、この重複したI/Oを避ける層です。
同じ50回の参照をBuffer Pool経由で行うと、`io_count`は1のまま増えなくなることを、`src/buffer_pool.rs`に次のテストとして確かめます。

```rust
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
```

最初の1回だけがミスとしてディスクへ行き、残り49回はヒットとしてメモリ上のコピーを返します。
この章の残りは、この`misses: 1, hits: 49`という結果がどうやって成り立っているかを組み立てていく作業です。

## ページをフレームに保持するBufferPoolの構造

Buffer Poolの仕組み自体は単純です。
決まった枚数のページを収められる**フレーム**という区画をあらかじめ用意しておき、参照されたページをそこへコピーして保持します。
どのページがどのフレームに入っているかを`page_table`という対応表で覚えておけば、次に同じページが参照されたとき、`DiskManager`まで行かずにフレームの中身をそのまま返せます。

フレームの枚数は`BufferPool::new`で決めた`capacity`のまま固定です。
テーブルが100ページに育っても、Buffer Poolのフレームが4枚しかなければ、同時にメモリへ載っているのは高々4ページ分だけです。
5ページ目を読み込みたいとき、Buffer Poolはどれか1枚のフレームを空けてから読み込みます。
これを**evict**(追い出し)と呼びます。
「今使っていないフレームをどうやって選ぶか」がこの章の主題の1つで、その選び方は後の節で説明する**Clock置換**が担います。

フレームには、ページの中身そのものに加えて、evictしてよいかどうかを判断するためのメタデータが必要です。
`src/buffer_pool.rs`に、次の`Frame`と`FrameMeta`を定義します。

```rust
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
}
```

`pin_count`が、この章の設計で最も重要な値です。
ページを参照している最中に、そのページがevictされてメモリから消えてしまっては困ります。
`pin_count`が1以上のフレームをBuffer Poolが**pin**中と呼び、pin中のフレームはevictの候補から常に除外します。
`dirty`は、そのページの中身がディスク上のものと食い違っている可能性があることを示すフラグで、evict時にこのフラグが立っているページだけを書き戻します。
`referenced`はClock置換が使う参照ビットで、次の節で扱います。

## pinがページを守り、GuardがpinをRAIIで管理する

`pin_count`という仕組みそのものは、Rust特有のものではありません。
C++で書かれた多くのデータベース実装でも同じ発想の`pin_count`が使われており、そこでは`pin(page_id)`でカウントを増やし、使い終わったら呼び出し側が責任を持って`unpin(page_id)`を呼んでカウントを減らします。
この方式の弱点は、`unpin`の呼び忘れです。
早期リターンや例外(Rustで言えば`?`によるエラー伝播)のある経路で1箇所でも`unpin`を書き忘れると、そのフレームは永遠にpinされたまま残り、Buffer Poolの実質的な容量がじわじわと減っていきます。
しかも、このバグは大抵の入力では発現せず、Buffer Poolがちょうど溢れるくらいの負荷がかかったときにだけ表面化するため、原因を特定するのが厄介です。

この章の`BufferPool`は、`unpin`という関数を呼び出し側に公開しません。
代わりに、`src/buffer_pool.rs`の`read_page`と`write_page`は、ページの中身へのアクセスをそれぞれ`PageReadGuard`と`PageWriteGuard`という値として返します。

```rust
pub fn read_page(&self, id: PageId) -> DbResult<PageReadGuard<'_>> {
    let frame_id = self.locate_and_pin(id)?;
    let guard = self.lock_frame(frame_id);
    Ok(PageReadGuard {
        pool: self,
        frame_id,
        page_id: id,
        guard,
    })
}
```

`PageReadGuard`は、`src/buffer_pool.rs`で`Drop`を実装した構造体です。

```rust
impl Drop for PageReadGuard<'_> {
    fn drop(&mut self) {
        self.pool.unpin(self.frame_id, false);
    }
}
```

呼び出し側がこのGuardをスコープの外に出す、つまり変数を明示的に`drop`する、ブロックを抜ける、`Vec`から取り除く、といった通常のRustの所有権の操作をすれば、その瞬間に`unpin`が呼ばれます。
呼び出し側が意識して`unpin`を呼ぶ操作は一切ありません。
早期リターンで関数を抜けても、`?`でエラーを伝播させても、Rustのコンパイラが保証する変数のドロップの規則にそのまま従うので、pinの解除だけが忘れられるという事態が起こりません。
`unpin`の呼び忘れは、「プログラマが手続きを1つ忘れる」という規律の問題から、「変数を生かしたままにする」という、コンパイラ自身が型を見て検査できる話に変わっています。

書き込み用の`PageWriteGuard`も、同じ`src/buffer_pool.rs`に同じ形で定義されていますが、`Drop`の中身が違います。

```rust
impl Drop for PageWriteGuard<'_> {
    fn drop(&mut self) {
        self.pool.unpin(self.frame_id, true);
    }
}
```

`PageWriteGuard`をDropすると、pinを外すのと同時にdirty flagを立てます。
実際にバイト列を書き換えたかどうかにかかわらず、`write_page`で取得したGuardは無条件にdirty扱いになります。
変更した範囲だけを正確に追跡する仕組みを持たない、この章の割り切りです。
`PageReadGuard`と`PageWriteGuard`の違いは、`data_mut`という可変アクセス用のメソッドを持つかどうかと、この`Drop`の中身だけです。
読み取り側は、`src/buffer_pool.rs`のテストにあるように`read_page`で取得し、`data`で中身を読みます。

```rust
let g1 = pool.read_page(PageId(1)).unwrap();
assert_eq!(&g1.data()[0..5], b"alice");
```

書き込み側は、同じ`src/buffer_pool.rs`のテストで`write_page`を使って取得し、`data_mut`で書き換えます。

```rust
let mut g = pool.write_page(PageId(1)).unwrap();
g.data_mut()[0..5].copy_from_slice(b"alice");
```

読み取りだけで済むはずの箇所に誤って`write_page`を書いてしまうと、実際には何も変更していないページまでdirty扱いになり、evict時に不要な書き戻しが発生します。
逆に、書き換えたいのに`read_page`しか呼んでいなければ、そもそも`data_mut`が存在しないためコンパイルが通りません。
読み取りと書き込みの意図を取り違えるバグも、実行時ではなく型の不一致としてコンパイル時に弾かれます。

## Clock置換でevictするフレームを選ぶ

空きフレームがなく、pin中でもないフレームが複数あるとき、Buffer Poolはそのうちのどれをevictするかを決めなければなりません。
最も単純な発想は、最後に参照されてから最も長く経っているフレームを選ぶ**LRU**(Least Recently Used)ですが、参照のたびに順序を管理するリストを更新するコストがかかります。
この章では、そのコストを1ビットの参照フラグ(`referenced`)で近似する**Clock置換**を使います。

Clock置換は、全フレームを環状に並べ、`clock_hand`という針で1つずつ調べていきます。
フレームが参照される(pinされる)たびに、そのフレームの`referenced`ビットを立てます。
針がフレームに差しかかったとき、次の3通りに分かれます。

- **pin中**: evictの候補になりえないので、`referenced`ビットには触れずそのまま次へ進みます。
- **`referenced`が立っている**: 「最近参照された」という情報を1回だけ消費し、ビットを倒して次へ進みます(2度目に針が巡ってきたときは、もう`referenced`は立っていないので、そのときは容赦なくevictされます)。
- **`referenced`が立っていない、かつpin中でもない**: このフレームをevictします。

`src/buffer_pool.rs`の`evict`は、この3通りの判定をそのまま実装します。

```rust
fn evict(&self, inner: &mut Inner) -> DbResult<usize> {
    let capacity = self.frames.len();
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
        if meta.dirty {
            let frame = self.lock_frame(i);
            if let Some(page) = frame.page.as_ref() {
                self.disk.write_page(page)?;
            }
        }
        self.lock_frame(i).page = None;
        inner.page_table.remove(&evicted_id);
        return Ok(i);
    }
    Err(DbError::BufferPoolFull(
        "全フレームがpin中のため、evictできるページがありません".to_string(),
    ))
}
```

ループの上限を`2 * capacity`にしているのは、この探索が高々2周で終わることを保証するためです。
1周目でpin中でない全フレームの`referenced`ビットを倒し終え、2周目でそのビットが(倒された状態のまま)残っているフレームに再び出会えば、それが2周のあいだ一度も参照されなかったフレームだとわかります。
この上限の範囲でevict候補が1つも見つからなければ、全フレームがpin中だということです。
その場合は`DbError::BufferPoolFull`を返します。
容量に対してpinしたままのページが多すぎる呼び出し側の使い方そのものが誤りであり、Buffer Poolが黙って動作を続けるべきではありません。

このことを、`src/buffer_pool.rs`に次のテストとして確かめます。

```rust
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
```

容量2のプールに2枚pinした状態で3枚目を要求すると`BufferPoolFull`になります。
ここで`g1`をdropしてpinを1つ外せば、同じ3枚目の要求は成功に変わります。
`unpin`を呼び忘れる余地がないGuardの設計は、この「pinを外せば次の参照が通る」という挙動を、呼び出し側が何も意識せずに得られることを意味します。

## メタデータとページ本体を別のMutexで守る

`src/buffer_pool.rs`では、`FrameMeta`(pin_count、dirty、referenced、occupant)と、フレームが持つページ本体(`Frame`)を、あえて別々の`Mutex`で守っています。

```rust
struct Inner {
    /// どのページがどのフレーム番号に読み込まれているか。
    page_table: HashMap<PageId, usize>,
    meta: Vec<FrameMeta>,
    /// 次にClock置換の候補として調べるフレーム番号。
    clock_hand: usize,
    hits: u64,
    misses: u64,
}
```

この`Inner`全体を、`src/buffer_pool.rs`の`BufferPool`は1本の`Mutex`で持ちます。

```rust
pub struct BufferPool {
    disk: DiskManager,
    /// フレームの配列。`new`で決めた容量のまま、以後は要素数を変えない。
    frames: Vec<Mutex<Frame>>,
    inner: Mutex<Inner>,
}
```

`PageReadGuard`と`PageWriteGuard`は、`data`、`data_mut`が返す参照の安全性を保証するために、`frames[frame_id]`の`Mutex`をGuardが生きている間ずっと握り続けます。
最初の実装では、`pin_count`や`referenced`もこの同じ`Mutex`の中に置いていました。
ところが、空きフレームを探す処理とClock置換の走査は、pin中のフレームも含めて全フレームの`pin_count`を確かめる必要があります。
`pin_count`がフレーム本体と同じ`Mutex`の中にあると、この走査はpin中のフレームの`Mutex`まで一度ロックしようとしてしまい、そのフレームをまさに使っているGuardが同じスレッドの中でそのロックをすでに握っている、という状況にぶつかります。
`std::sync::Mutex`は再入可能ではないので、同じスレッドが同じ`Mutex`を二重にロックしようとすると、そのままロックが返ってこなくなります。
実際、この章の実装を書く過程で、pinしたページを2枚保持した状態で3枚目を要求するテストがまさにこの経路で無限に停止し、修正が必要になりました。

修正の方向は、メタデータをフレーム本体から切り離すことでした。
`pin_count`、`dirty`、`referenced`、`occupant`を`Inner`という別の構造体にまとめ、`frames`とは別の`Mutex<Inner>`で守ります。
空きフレーム探しとClock置換の走査は、この軽い`Inner`だけを見て判断し、フレーム本体の`Mutex`には一切触れません。
フレーム本体をロックするのは、evictするフレームが確定した後にそのページを書き戻す、空にする瞬間と、新しいページを読み込んで書き込む瞬間だけです。
このとき対象になるフレームは、直前に`meta[i].pin_count == 0`だと`Inner`側で確認済みなので、pin中のGuardと衝突することはありません。

## HeapFileをBufferPool経由に置き換える

第13章の`HeapFile`は`DiskManager`を直接保持していました。
この章では、その保持先を`BufferPool`に置き換えます。
`src/heap_file.rs`の`HeapFile`定義を、次のように書き換えます。

```rust
pub struct HeapFile {
    pool: BufferPool,
    /// このテーブルが使っているデータページの一覧(挿入順ではなく、
    /// ファイル中のページ番号順)。
    page_ids: Vec<PageId>,
}
```

置き換えの影響が最も大きいのは`insert`です。
第13章では「`disk.read_page`でページを取り出し、書き換えてから`disk.write_page`で書き戻す」という2段階の手続きでしたが、`src/heap_file.rs`のこの`insert`では書き戻しの手続きそのものが消えます。

```rust
pub fn insert(&mut self, bytes: &[u8]) -> DbResult<RecordId> {
    if bytes.len() > max_len_for_fresh_page(PAGE_PAYLOAD_SIZE) {
        return Err(DbError::TupleTooLarge(bytes.len()));
    }

    for &page_id in &self.page_ids {
        let mut guard = self.pool.write_page(page_id)?;
        if let Some(slot) = SlottedPage::open(guard.data_mut())?.insert(bytes) {
            return Ok(RecordId::new(page_id, slot));
        }
    }

    let page_id = self.pool.allocate_page(PageType::Data)?;
    let mut guard = self.pool.write_page(page_id)?;
    let slot = SlottedPage::init(guard.data_mut())
        .insert(bytes)
        .ok_or(DbError::TupleTooLarge(bytes.len()))?;
    drop(guard);
    self.page_ids.push(page_id);
    Ok(RecordId::new(page_id, slot))
}
```

先頭の事前検査(第12章の`max_len_for_fresh_page`)は第13章から変わっていません。
これがないと、失敗するだけの`insert`のたびに`allocate_page`が呼ばれ、`BufferPool`を経由するようになったこの章でもファイルは同じように肥大化します。

`guard.data_mut()`を`SlottedPage::open`に渡して書き換えれば、それで作業は終わりです。
`guard`がスコープを抜ける(このループの各反復の終わり、または関数の終わり)ときに、`PageWriteGuard`の`Drop`がpinを外すと同時にdirty flagを立てます。
そのページが実際にディスクへ書き戻されるのは、evictされる瞬間か、`flush`が明示的に呼ばれた瞬間です。

`get`の側には、もう1つの壁がありました。
`SlottedPage::open`(第12章)は`&mut [u8]`を要求する設計になっており、これは`get`のような読み取り専用の操作でも変わりません。
`PageReadGuard`は、可変な参照を外へ渡さないことによって「このGuardを`Drop`してもdirty flagは立たない」という約束を型で保証しているGuardです。
`PageReadGuard`から`&mut [u8]`を取り出せる抜け道を1つでも用意すれば、その抜け道を経由して書き換えられたページはdirty扱いにならないまま、実際には中身が変わっているという状態を作れてしまいます。
`HeapFile`の内部専用だからと言って、crateの外から見えないだけでこの抜け道を空けてよい理由にはなりません。
crateの中には`get`、`scan`だけでなく、将来のB+Treeの検索のような、同じく読み取り専用のまま`SlottedPage`相当の構造を読みたいコードが他にも増えていくからです。

この壁を壊す方法は、`PageReadGuard`側に抜け道を空けることではなく、`SlottedPage`(第12章)の側に読み取り専用の入口を追加することでした。
`src/slotted_page.rs`に、次の`SlottedPageRef`を追加します。

```rust
pub struct SlottedPageRef<'a> {
    payload: &'a [u8],
}
```

`SlottedPageRef`は`&'a mut [u8]`ではなく`&'a [u8]`だけを借用し、`get`、`slot_count`、`free_space`、`status`という読み取り系のメソッドだけを持ちます。
`insert`、`delete`、`update`、`compact`のような書き込み系のメソッドは最初から存在しないため、`SlottedPageRef`をどれだけ経由しても`payload`を書き換えるコードを書きようがありません。
`src/slotted_page.rs`では、ヘッダーやスロットエントリを読む下請けのロジック(`read_header`、`read_slot_entry`)を`SlottedPage`と`SlottedPageRef`の両方から呼ばれる自由関数として切り出してあり、`payload`が可変か不変かでロジックが重複することはありません。

```rust
pub fn get(&self, slot: SlotId) -> Option<&[u8]> {
    let (offset, length, status) = read_slot_entry(self.payload, slot)?;
    if status != STATUS_OCCUPIED {
        return None;
    }
    let start = offset as usize;
    Some(&self.payload[start..start + length as usize])
}
```

`HeapFile::get`は、`PageReadGuard::data()`が返す`&[u8]`をそのまま`SlottedPageRef::open`に渡すだけになります。
`src/heap_file.rs`の`get`をこう書き換えます。

```rust
pub fn get(&self, rid: RecordId) -> DbResult<Option<Vec<u8>>> {
    let guard = self.pool.read_page(rid.page_id)?;
    Ok(SlottedPageRef::open(guard.data())?
        .get(rid.slot_id)
        .map(|bytes| bytes.to_vec()))
}
```

`guard`はもう`mut`である必要がありません。
`PageReadGuard`と`PageWriteGuard`を分けている本質は、こうして「可変な参照を返せるかどうか」で表現できるようになりました。
`src/heap_file.rs`の`update`が、存在確認のためだけに`write_page`を呼ばないようにしているのも同じ設計の延長です。

```rust
pub fn update(&mut self, rid: RecordId, bytes: &[u8]) -> DbResult<Option<RecordId>> {
    // 対象が存在するかどうかは読み取り専用のGuardで確かめる。存在しない
    // 場合にまで`write_page`でpinしてdirty扱いにしてしまうと、evict時の
    // 無駄な書き戻しが増える。
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
            return Ok(Some(rid));
        }
        // このページの中には(コンパクションしても)収まらない。ここでは
        // まだ元の行を削除しない(モジュールのドキュメントを参照)。
    }

    let new_rid = self.insert(bytes)?;
    match self.delete(rid) {
        Ok(true) => Ok(Some(new_rid)),
        Ok(false) => {
            // 直前にoccupiedを確認済みで、この章はシングルスレッド前提
            // なので通常は起こらない。万一起きた場合は、すでに書き込んだ
            // 新しい行をロールバックしてから異常として報告する。
            let _ = self.delete(new_rid);
            Err(DbError::CorruptPage(format!(
                "update: 元のRecordId({rid:?})の削除に失敗しました(想定外)"
            )))
        }
        Err(err) => {
            let _ = self.delete(new_rid);
            Err(err)
        }
    }
}
```

`SlottedPage::open`と`SlottedPageRef::open`の戻り値が`DbResult<Self>`である(第12章)ため、ここでも`?`で受けています。

もう1つ、第13章で見た版(このページに収まらないと分かった時点で、その場で元のスロットを`delete`してから`insert`し直す版)との違いにも気づいたかもしれません。
この章の実装は、このページに収まらないと分かった時点ではまだ元の行を削除しません。
先に削除してから`self.insert(bytes)`する順序を選んでいたら、その`insert`が`DbError::TupleTooLarge`(`bytes`自体がどのページにも収まらないほど大きい)で失敗したときに、元の行はすでに削除済みという状態になってしまいます。
`UPDATE`が失敗として呼び出し元へ`Err`を返しているのに、対象の行は消えている。
これは受け入れられない振る舞いです。
挿入を先に試すことで、挿入が失敗した時点では元の行がまだ手つかずのまま残ります。
挿入が成功したあとの`self.delete(rid)`がもし失敗したとき(この章の設計ではシングルスレッド前提のため通常は起こりませんが)は、直前に挿入した新しい行を削除してロールバックしたうえで異常として報告します。

対象のスロットが存在するかどうかは`read_page`で確かめ、実際に書き換える段になって初めて`write_page`に切り替えます。
存在しない`rid`を渡された`update`のたびに`write_page`を呼んでいたら、何も書き換えていないページまでdirtyになり、evictのたびに無駄な書き戻しが発生するところでした。

置き換えの結果、`insert`、`update`、`delete`という公開シグネチャそのものは変わっていません。
変わったのは`HeapFile::open`が`DiskManager`ではなく`BufferPool`を受け取る点だけです。
呼び出し側が意識する必要のある変更点はもう1つあります。
`BufferPool`はdirtyなページを明示的に`flush`するまでディスクへ書き戻しません。
第13章の`HeapFile`は`insert`のたびに`disk.write_page`を呼んでいたので、事実上つねに書き込み済みでしたが、この章の`HeapFile`はそうではありません。
プロセスの再起動をまたいでデータを残したいコードは、`HeapFile::flush`を呼んでからファイルを閉じる必要があります。
このテストは`src/heap_file.rs`の`#[cfg(test)] mod tests`に置きます。

```rust
#[test]
fn reopening_the_disk_manager_preserves_the_heap_file_contents() {
    let path = temp_path("reopen");
    let mut inserted = Vec::new();
    {
        let disk = DiskManager::open(&path).unwrap();
        // page_ids::pushで容量を使い切らないよう、十分な容量を確保する。
        let mut heap = HeapFile::open(BufferPool::new(disk, 8));
        for i in 0..300u32 {
            let bytes = format!("row-{i:04}").into_bytes();
            let rid = heap.insert(&bytes).unwrap();
            inserted.push((rid, bytes));
        }
        // BufferPoolはdirtyなページを明示的にflushするまで書き戻さない。
        // 第13章のDiskManager::syncと同様、書き戻し自体はheap(と、その中の
        // BufferPool)がスコープを抜けてdropされる前に呼んでおく必要がある。
        heap.flush().unwrap();
        // heapはここでスコープを抜けてdropされる(closeに相当)。
    }

    let disk = DiskManager::open(&path).unwrap();
    let heap = HeapFile::open(BufferPool::new(disk, 8));
    let scanned: Vec<_> = heap.scan().collect::<DbResult<Vec<_>>>().unwrap();
    assert_eq!(scanned.len(), inserted.len());
    for (rid, bytes) in &inserted {
        assert_eq!(heap.get(*rid).unwrap(), Some(bytes.clone()));
    }

    std::fs::remove_file(&path).unwrap();
}
```

`heap.flush()`を削除すると、この場合は`BufferPool`の容量(8フレーム)がテーブルの全ページ数を上回っているため、1つもevictが起きないままプロセス内メモリにdirtyなページが残り続け、再起動後に変更が失われます。
第13章で`disk.sync()`を省いたときと同様、明示的な書き戻しをどこかで行うかどうかは、この章でもまだ呼び出し側の責任です。
コミットのたびにログを先に書き、そのログさえ残っていればページの内容は再構築できるという設計は、第33章のWrite-Ahead Loggingで扱います。

## dirtyなページはevict時にも書き戻される

`flush`を呼ばなくても、容量を超えてページを参照し続ければ、evictのタイミングで自動的に書き戻しは起こります。
このテストは`src/buffer_pool.rs`の`#[cfg(test)] mod tests`に戻ります。

```rust
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
```

容量2のプールにページ1を書き込んだ後、ページ2、ページ3と読み込んでいくと、ページ1は(Clock置換によって)どこかの時点でevictされます。
このとき`meta.dirty`が立っているので、evictの直前に`DiskManager::write_page`が呼ばれ、`alice`という変更はディスク上に残ります。
その後もう一度ページ1を参照すると、それはBuffer Poolにとってはミス(ディスクからの読み直し)になりますが、読み直した内容にはさきほどの変更がちゃんと反映されています。
呼び出し側は、どのページがいつevictされたかを一切気にする必要がありません。

## テストで確認する

ここまでの`src/buffer_pool.rs`のテストは、ヒット/ミス、容量超過によるeviction、dirtyな変更の書き戻し、全pin時のエラー、Guardのdropによるunpinという5つの観点をそれぞれ独立したテストとして持っています。

```rust
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
```

このテストは、evictされるのがページ1とページ2のどちらであるかを固定していません。
`referenced`ビットの扱いにより、Clock置換の候補は針の位置に依存しますが、どちらがevictされるにせよ「容量を超えて3枚目を読み込むと、既存の2枚のうち少なくとも1枚は必ず追い出される」という性質は変わりません。
テストはこの不変条件だけを、`misses`が確かに1つ増えることによって確認しています。

Guardのdropが確実にunpinすることは、`src/buffer_pool.rs`の`pinning_beyond_capacity_returns_buffer_pool_full`の続きとして確認できます。

```rust
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
```

容量2のプールを2枚のGuardで埋めた状態では3枚目の参照が失敗し、片方をdropした後は成功します。
`unpin`という関数を呼び出すコードがどこにも書かれていないにもかかわらず、この挙動が成り立つことこそ、この章がGuardに込めた設計そのものです。

`heap_file`モジュールのテストは、第13章のものをほぼそのまま引き継いでいます。
`insert`、`get`、`update`、`delete`、`scan`の挙動、複数ページにまたがる走査、ページをまたぐ`update`によるRecordIdの変化は、第13章と同じ結果になることをそのまま確認しています。
違うのは、`open_heap`ヘルパーが`DiskManager`を直接ではなく`BufferPool::new(disk, 16)`を経由して`HeapFile::open`に渡す点と、再起動を確認するテストが`heap.flush()`を挟む点だけです。
Buffer Poolの容量がテーブルのページ数を下回っていても正しく動くことも、別に確認しています。
このテストも`src/heap_file.rs`の`#[cfg(test)] mod tests`に置きます。

```rust
#[test]
fn heap_file_works_with_a_buffer_pool_smaller_than_the_page_count() {
    // BufferPoolの容量がテーブルのページ数より小さくても、eviction
    // 経由で正しく動作することを確認する(HeapFile自体はBufferPoolの
    // 容量を意識しない)。
    let path = temp_path("small-pool");
    let disk = DiskManager::open(&path).unwrap();
    let mut heap = HeapFile::open(BufferPool::new(disk, 2));

    let mut inserted = Vec::new();
    for i in 0..500u32 {
        let bytes = format!("row-{i:04}").into_bytes();
        let rid = heap.insert(&bytes).unwrap();
        inserted.push((rid, bytes));
    }
    assert!(heap.page_ids().len() > 1);

    for (rid, bytes) in &inserted {
        assert_eq!(heap.get(*rid).unwrap(), Some(bytes.clone()));
    }

    std::fs::remove_file(&path).unwrap();
}
```

容量2のBuffer Poolに、500件のタプルが収まる何十ページものテーブルを載せています。
`insert`のたびにページが入れ替わり立ち替わりevictされますが、`HeapFile`自身はどのページが今メモリに載っているかをまったく気にせず、`page_id`をそのまま`pool.read_page`、`pool.write_page`に渡すだけです。
Buffer Poolの容量とテーブルの大きさが無関係に扱えることは、この章の設計が達成したかったことの1つです。

## 到達点

この章で`DiskManager`と`HeapFile`の間に挟んだ`BufferPool`によって、同じページへの繰り返しの参照はもうディスクI/Oを起こさなくなりました。
章の冒頭で確かめた「50回参照すれば50回ディスクへ行く」という状態は、「最初の1回だけがディスクへ行き、残り49回はメモリ上のフレームを返す」という状態に変わっています。
`pin_count`とGuardの組み合わせにより、この置き換えは呼び出し側の設計を複雑にしていません。
`HeapFile`の公開シグネチャは、`open`が`BufferPool`を受け取るようになった点を除けばそのままで、内部の`read_page`、`write_page`の呼び出しからは、明示的な`unpin`も明示的な`write_page`(書き戻し)の呼び出しも消えています。

この章の`BufferPool`にはまだ残された割り切りがいくつかあります。
1つは、フレームごとの`Mutex`を1つのGuardが専有する設計上、同じページを2つのGuardで同時にpinすることは(単一スレッドの中であっても)サポートしていない点です。
`HeapFile`のどの操作も同じページを同時に2度pinすることはないため、この章の範囲では問題になりませんが、複数のカーソルが同じページを同時に参照するJoinのような操作には、この章のままでは対応できません。
もう1つは、`page_table`の更新とフレームへの書き込みを1つの操作として原子的に行う保証がなく、複数スレッドからの同時アクセスにまだ対応していない点です。
この2つの限界は、ページ単位の細かい排他制御である**Latch**を導入する第35章で解消します。
第15章では、この章までの「1ファイル1テーブル」という制約を外し、複数のテーブルを1つのファイルに共存させるカタログと、`HeapFile::insert`が行っている線形探索を置き換えるFree Space Mapを導入します。

## 演習問題

### 必須課題

1. `PageWriteGuard`は、実際にバイト列を変更したかどうかにかかわらず、`Drop`時に無条件でdirty flagを立てます。`HeapFile::update`が対象の存在確認を`read_page`で行い、`write_page`は実際に書き換える段になってから呼んでいる理由を、`dirty_page_is_written_back_on_eviction`テストの構造を踏まえて説明してください。
2. `BufferPool::evict`のループ上限が`2 * capacity`になっている理由を、`referenced`ビットが最悪の場合何回倒され直すかという観点から説明してください。上限を`capacity`(1周分)に減らした場合、どのような入力でevictに失敗するようになるか、具体例を考えてください。
3. `pinning_beyond_capacity_returns_buffer_pool_full`テストから`drop(g1); drop(g2);`を削除すると、テスト自体はどうなるか(コンパイルは通るか、実行結果は変わるか)を確認し、その理由を説明してください。

### 発展課題

1. この章の`BufferPool`は、フレームのメタデータ(`pin_count`、`dirty`、`referenced`、`occupant`)を`Mutex<Inner>`に、フレーム本体を`Vec<Mutex<Frame>>`に分けて持っています。もしこの分離をせず、両方を1つの`Mutex<Frame>`にまとめていたら、どのテストがどのように失敗する(あるいは無限に停止する)か、具体的な呼び出し順序を1つ構成して説明してください。
2. 現在の設計では、同じページを2つの`PageReadGuard`で同時にpinすることはできません(2つ目の`read_page`呼び出しがフレームの`Mutex`のロック待ちで止まります)。これを可能にするには、フレーム本体を守る型を`Mutex<Frame>`から何に変える必要があるか考えてみてください。`PageReadGuard::data()`は`SlottedPageRef`経由の読み取りしか必要としない一方、`PageWriteGuard::data_mut()`は`SlottedPage::open`の`&mut [u8]`をそのまま要求します。この非対称さが、その型をどう選ぶかにどう影響するか説明してください。
3. `BufferPool::flush_all`は、`Inner`のロックとフレームのロックを同時に持たないように実装されています。もし`Inner`のロックを取ったままループの中で各フレームをロックする実装に書き換えたら、どのような呼び出し順序でデッドロックが起こりうるか、具体例を考えてください。
