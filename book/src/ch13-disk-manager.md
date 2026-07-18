# 第13章 Disk ManagerとHeap File

前章のテストをもう一度眺めてみます。

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

`fresh_payload`が返すのは、ただの`Vec<u8>`です。
`SlottedPage::init`はそれを借用してスロットの並びとして読み書きするだけで、関数を抜ければ`payload`ごとメモリから消えます。
第11章の`Page::encode`も第12章の`SlottedPage`も、バイト列をどう組み立てるかを決めた部品であって、そのバイト列をどこかのファイルへ実際に送り届ける処理はまだ1行も書いていません。
`minidb`を終了して起動し直しても`users`が残っている、という第11章冒頭で立てた目標に対して、この章までの成果物はまだ何の役にも立っていないことになります。

この章では、その「バイト列を実際のファイルへ送り届ける」部分をDisk Managerとして実装します。
そのうえで、複数のページにまたがる1つのテーブルをHeap Fileとして扱えるようにし、`INSERT`、`SELECT`、`UPDATE`、`DELETE`に相当する操作をページの読み書きの上に組み立てます。
SQLの実行経路をこのHeap Fileへ実際に差し替えるのは第16章の仕事なので、この章の時点では`minidb`のSQL文はまだ第10章の`MemStorage`を使い続けます。

## ページを実ファイルへ読み書きするDiskManagerを設計する

Disk Managerの役割は、「`N`番目のページを読む」「`N`番目のページへ書く」という要求を、実際のファイルの決まった位置への読み書きに変換することです。
第11章で決めたとおり、ファイルは`PAGE_SIZE`バイトごとの区画の並びであり、`N`番目のページはファイル先頭から`N * PAGE_SIZE`バイト目にあります。
この対応がある以上、Disk Managerの仕事のほとんどは、ページ番号をファイル中のオフセットへ変換し、そこへ`seek`してから読み書きするという単純な手続きです。

もう1つ、この章で決めておくレイアウト上の約束があります。
ファイルの先頭のページ(ページ0)を、`FileHeader`を保持する専用のページとして扱うという約束です。
第11章の`PageType`にはすでに`Meta`という値が用意されていて、この章で初めて使います。

```text
offset 0                 PAGE_SIZE               2*PAGE_SIZE
+------------------------+------------------------+-----
| Page 0 (Meta)          | Page 1 (Data)          | ...
| payload先頭にFileHeader |                        |
+------------------------+------------------------+-----
```

`FileHeader`自体は24バイトしかありませんが、ページ0の残りの4056バイトを別の用途に使う仕組みはまだこの章にはないので、ページ0の`payload`の先頭24バイトだけを使い、残りは0で埋めておきます。
`FileHeader::page_count`は「File Header自身を含むページの総数」と定義されていたことを思い出すと、ページ0を含めてページを数えるこの構成と矛盾しません。
正常なファイルのバイト数は、常に`page_count * PAGE_SIZE`と一致します。

この構造を、`DiskManager`という1つの構造体にまとめます。

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

`Inner`をまとめて保持する`DiskManager`自体は、次のような形をしています。

```rust
pub struct DiskManager {
    inner: Mutex<Inner>,
}
```

`file`と`page_count`を`Inner`という別の構造体にまとめ、それを`Mutex`で包んでいる理由は、あとの節で説明します。
まずは`page_count`という値の役割を見ておきます。
`page_count`はファイル先頭の`FileHeader`が持つ値そのものですが、`read_page`や`allocate_page`のたびにページ0を読み直して`page_count`を確かめるのは無駄です。
`DiskManager`はこの値をメモリ上にも保持しておき、`FileHeader`を更新するときにこの値も一緒に書き換えます。

## ファイルを開くときにFile Headerを検証する

`DiskManager::open`は、指定したパスのファイルが存在しなければ新規作成し、存在すればその中身を検証してから開きます。

```rust
pub fn open<P: AsRef<Path>>(path: P) -> DbResult<Self> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;

    let len = file.seek(SeekFrom::End(0))?;
    let page_count = if len == 0 {
        Self::init_new_file(&mut file)?
    } else {
        Self::verify_existing_file(&mut file, len)?
    };

    Ok(DiskManager {
        inner: Mutex::new(Inner { file, page_count }),
    })
}
```

ファイルサイズが0であれば、まだ何も書き込まれていない新規のファイルだと判断し、ページ0にMetaページを書き込みます。

```rust
fn init_new_file(file: &mut File) -> DbResult<u64> {
    let page_count = 1;
    let mut meta = Page::new(META_PAGE_ID, PageType::Meta);
    let header = FileHeader::new(page_count);
    meta.payload_mut()[0..crate::page::FILE_HEADER_SIZE].copy_from_slice(&header.encode());

    file.seek(SeekFrom::Start(0))?;
    file.write_all(&meta.encode())?;
    file.sync_all()?;
    Ok(page_count)
}
```

`page_count`を1で初期化しているのは、Metaページ自身をページ0として数えるためです。
この時点ではまだデータページが1枚もないので、テーブルに行を1件も持たないファイルの`page_count`は1になります。

ファイルサイズが0でなければ、そのファイルはすでに`minidb`が(あるいは別の何かが)書き込んだ既存のファイルです。
中身を無条件に信用せず、ページ0を読んで`FileHeader`を検証してからページ数を確定します。

```rust
fn verify_existing_file(file: &mut File, len: u64) -> DbResult<u64> {
    file.seek(SeekFrom::Start(0))?;
    let mut buf = [0u8; PAGE_SIZE];
    file.read_exact(&mut buf)?;
    let meta = Page::decode(&buf)?;

    if meta.page_type != PageType::Meta {
        return Err(DbError::CorruptPage(format!(
            "ページ0はMetaページである必要がありますが{:?}でした",
            meta.page_type
        )));
    }

    let header = FileHeader::decode(&meta.payload()[0..crate::page::FILE_HEADER_SIZE])?;
    if header.page_size as usize != PAGE_SIZE {
        return Err(DbError::CorruptPage(format!(
            "page_sizeが一致しません: {PAGE_SIZE}が期待されましたが{}でした",
            header.page_size
        )));
    }

    let expected_len = header.page_count * PAGE_SIZE as u64;
    if len != expected_len {
        return Err(DbError::CorruptPage(format!(
            "ファイルサイズがpage_countと一致しません: page_count={}から期待される{expected_len}バイトに対し、実際のファイルは{len}バイトでした",
            header.page_count
        )));
    }

    Ok(header.page_count)
}
```

`Page::decode`と`FileHeader::decode`は第11章で実装済みなので、Magic Number、Format Version、checksumの検証はここでは呼び出すだけで済みます。
この関数が新たに加えている検証は2つです。
1つは`page_size`が現在の`PAGE_SIZE`と一致するかどうかで、これは`FileHeader`が保持している値そのものが正しいかを、ファイルの中身に対してではなく、このプロセスが動いている前提に対して確かめるものです。
もう1つは、実際のファイルサイズが`page_count * PAGE_SIZE`と一致するかどうかです。
これは`FileHeader`のchecksumだけでは検出できない壊れ方で、たとえば書き込みの途中でプロセスが強制終了し、あるページの一部しかディスクに届かなかった場合に起こりえます。
そのようなファイルは、ページ0の`FileHeader`自体は正しくても、末尾のページが中途半端な長さしか無いという矛盾を抱えており、この長さの突き合わせでしか検出できません。

## ページの読み書きにchecksumを組み込む

ファイルを開いたあとの本体は、`read_page`と`write_page`です。

```rust
pub fn read_page(&self, id: PageId) -> DbResult<Page> {
    let mut inner = self.lock();
    Self::check_range(id, inner.page_count)?;

    let mut buf = [0u8; PAGE_SIZE];
    inner.file.seek(SeekFrom::Start(Self::offset(id)))?;
    inner.file.read_exact(&mut buf)?;
    Page::decode(&buf)
}
```

`read_page`がまず行うのは、`id`が現在の`page_count`の範囲内かどうかの確認です。

```rust
fn check_range(id: PageId, page_count: u64) -> DbResult<()> {
    if id.0 >= page_count {
        return Err(DbError::PageOutOfRange(format!(
            "PageId({})はページ数{page_count}の範囲外です",
            id.0
        )));
    }
    Ok(())
}
```

まだ`allocate_page`されていないページ番号を指定してしまうバグは、この章より先の章で必ず一度は起こります(実際、この章のHeap Fileの実装でも、ページを割り当てる前にそのIDへアクセスしないよう順序に気を配る必要がありました)。
範囲外のアクセスを、ファイルサイズを超える`seek`や中途半端な`read`として起こすのではなく、`DbError::PageOutOfRange`という1つの分かりやすいエラーとしてここで弾いておくと、バグの原因がすぐに分かります。

範囲チェックを通過したら、該当するオフセットから`PAGE_SIZE`バイトを読み込み、`Page::decode`に渡します。
`Page::decode`は第11章の実装のままで、checksumが一致しなければ`DbError::CorruptPage`を返します。
`DiskManager`はこの検証を自分で書き直さず、`Page`にすでにある実装をそのまま呼び出すだけです。
`write_page`も対称的な作りです。

```rust
pub fn write_page(&self, page: &Page) -> DbResult<()> {
    let mut inner = self.lock();
    Self::check_range(page.page_id, inner.page_count)?;

    inner.file.seek(SeekFrom::Start(Self::offset(page.page_id)))?;
    inner.file.write_all(&page.encode())?;
    Ok(())
}
```

`page.encode()`が、書き込む直前にchecksumを計算し直したバイト列を返します。
呼び出し側が`payload`の中身をどう書き換えていても、`write_page`に渡す前に自分でchecksumを計算し直す必要はありません。

新しいページを割り当てる`allocate_page`は、`read_page`や`write_page`とは違い、ファイルそのものを大きくします。

```rust
pub fn allocate_page(&self, page_type: PageType) -> DbResult<PageId> {
    let mut inner = self.lock();
    let new_id = PageId(inner.page_count);
    let new_page_count = inner.page_count + 1;

    let page = Page::new(new_id, page_type);
    inner
        .file
        .seek(SeekFrom::Start(Self::offset(new_id)))?;
    inner.file.write_all(&page.encode())?;

    let mut meta = Page::new(META_PAGE_ID, PageType::Meta);
    let header = FileHeader::new(new_page_count);
    meta.payload_mut()[0..crate::page::FILE_HEADER_SIZE].copy_from_slice(&header.encode());
    inner.file.seek(SeekFrom::Start(0))?;
    inner.file.write_all(&meta.encode())?;

    inner.page_count = new_page_count;
    Ok(new_id)
}
```

新しいページIDは、現在の`page_count`をそのまま使います。
ページはこれまで常に`0`から`page_count - 1`まで連続して埋まっているので、次に空いている番号は`page_count`そのものだからです。
新しいページを空の状態で書き込んだあと、`allocate_page`はページ0のMetaページも書き直しています。
ページが1枚増えたことは、ファイルの中身としては新しいページを書き込んだ時点ですでに起きていますが、そのページ数が増えたという事実そのものも、次に`DiskManager::open`でこのファイルを開いたときに`FileHeader`から読み取れなければなりません。
そのため、データページの書き込みとMetaページの書き直しの2回の書き込みを、1回の`allocate_page`呼び出しの中で行っています。

## write_pageが返ってきても、ディスクに届いたとは限らない

ここまでの`write_page`や`allocate_page`は、`Ok(())`を返した時点で仕事を終えたように見えます。
しかし、その`Ok(())`が意味するのは「OSに書き込みを渡した」ということだけです。

OSは、アプリケーションから`write`システムコールを受け取っても、そのバイト列を即座にディスクへ書き込むとは限りません。
多くの環境では、書き込まれたデータはまずページキャッシュと呼ばれるメモリ上の領域に置かれ、OSが都合の良いタイミングでまとめてディスクへ反映します。
アプリケーションから見ると、`write`はディスクI/Oよりずっと高速に返ってきますが、それはこのキャッシュのおかげです。
`write_page`が`Ok(())`を返した直後に電源が落ちれば、そのページの内容がディスク上にまだ反映されていない可能性があります。

この事情を扱うために用意するのが`sync`です。

```rust
pub fn sync(&self) -> DbResult<()> {
    let inner = self.lock();
    inner.file.sync_all()?;
    Ok(())
}
```

`File::sync_all`は、OSに対して「このファイルに対するこれまでの書き込みを、実際にディスクへ届けてから返ってきてほしい」と要求するシステムコールを発行します。
`sync_data`という、ファイルの中身だけを同期して更新日時のようなメタデータの同期を省く、より軽量な選択肢もありますが、この章では`sync_all`を使っています。
`allocate_page`はファイルサイズというメタデータ自体を変化させるため、中身だけを同期する`sync_data`では、そのメタデータの変更まで確実にディスクへ届く保証がないからです。

`sync`をどのタイミングで呼ぶべきかは、この章では踏み込みません。
`write_page`のたびに毎回`sync`を呼べば、少なくともそのページについては書き込み漏れの心配がなくなりますが、ディスクへの同期はページキャッシュへの書き込みよりずっと低速なので、行を1件書き込むたびに毎回そのコストを払うのは受け入れがたいことがほとんどです。
一方で、`sync`を呼ぶ回数を減らせば、その分だけクラッシュ時に失われうる書き込みの範囲が広がります。
このトレードオフに対する`minidb`の答えは、ログを先に書いてから同期し、そのログさえ残っていればページの内容は再構築できるという設計を組み立てる第33章のWrite-Ahead Loggingで扱います。
この章の`DiskManager`は、`sync`という手段だけを用意し、いつ呼ぶかの判断は呼び出し側(この章ではテストコード)に委ねます。

## DiskManagerを具象型として実装する理由

`DiskManager`をtraitにせず、ここまで見てきたとおり具象の構造体として実装しました。
第11章の`Page`も第12章の`SlottedPage`も、同じように具象型のままです。
この章まで`minidb`のストレージ層には、複数の実装を切り替える必要のある部品がまだ1つも登場していません。

この選択が、後の章と矛盾しないかどうかは確認しておく価値があります。
第14章のBuffer Poolは、ページをエビクションするときにこの`DiskManager`を読み書き先として使いますが、Buffer Poolが必要とするのは「ページを読み書きできる何か」であって、その実体が複数あることではありません。
第33章のWALでクラッシュ・リカバリのテストを書くころには、書き込みを意図的に失敗させられる偽の`DiskManager`が欲しくなる可能性があります。
そうなったとしても、`DiskManager`の`read_page`、`write_page`、`allocate_page`、`sync`という今のメソッド群からtraitを1つ切り出し、この`DiskManager`にそのtraitを実装させるだけで済みます。
呼び出し側のメソッド呼び出しは何も変わりません。

まだ存在しない要求のためにtraitを先取りしてしまうと、動的ディスパッチや型引数を、それを必要としている箇所が1つもないままBuffer Poolやテストコードにまで持ち込むことになります。
必要になった時点で、今の具象型からtraitを後付けで切り出す方が、今の時点で払う抽象化のコストより小さく済みます。

`read_page`、`write_page`、`allocate_page`のシグネチャが`&mut self`ではなく`&self`になっている点にも、同じ先取りの発想が関わっています。
`file`と`page_count`を`Mutex<Inner>`にまとめているのはこのためで、各メソッドはそのロックを取ってから読み書きします。

```rust
fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
    self.inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
```

この章の`minidb`はまだシングルスレッドで動いており、並行アクセスは第35章のLatchまで登場しません。
それでも`&self`を選んでいるのは、第14章のBuffer Poolが`DiskManager`を`Arc`で複数のフレームから共有できるようにしておきたいからです。
`&mut self`のままでは、呼び出し側がその排他参照を1箇所でしか持てず、Buffer Poolの設計そのものを窮屈にしてしまいます。
`Mutex`によるロックは、全てのページI/Oを1本のロックで直列化するだけの素朴な実装であり、ページ単位の細かい並行性は持ちません。
シングルスレッドの間はロックの取り合いも起きないので、この単純さで困ることはありません。

## 複数ページの集まりとしてのHeap File

`DiskManager`はページ1枚を読み書きできますが、「`users`というテーブルはどのページに入っているか」を知りません。
この対応関係を管理するのがHeap Fileです。

```rust
pub struct HeapFile {
    disk: DiskManager,
    /// このテーブルが使っているデータページの一覧(挿入順ではなく、
    /// ファイル中のページ番号順)。
    page_ids: Vec<PageId>,
}
```

この章の`HeapFile`は、1つの`DiskManager`(1つのファイル)を丸ごと1個のテーブルとして占有します。

```rust
pub fn open(disk: DiskManager) -> Self {
    let page_ids = (1..disk.page_count()).map(PageId).collect();
    HeapFile { disk, page_ids }
}
```

`open`は、ページ0(Metaページ)を除く全ページを、このテーブルのデータページとみなして`page_ids`に加えます。
複数のテーブルを1つのファイルに共存させ、テーブルごとに「自分が使っているページ番号の一覧」をどこかに永続化しておく仕組みは、この章にはまだありません。
それを担うカタログとFree Space Mapは第15章で導入します。
この章の`HeapFile::open`が毎回ファイル全体を走査してページ一覧を作り直すのは、その仕組みが無い間の割り切りです。

この割り切りには利点もあります。
プロセスを再起動しても`HeapFile`の中身が残っていることを確認するテストは、この`open`の実装にそのまま乗ります。
`DiskManager`を閉じて同じパスへもう一度`open`し、`HeapFile::open`をもう一度呼べば、ページ0以降の全ページが自動的に見つかります。

行1件を指し示す方法も、第12章の`RecordId`(`PageId`と`SlotId`の組)をそのまま使います。
`HeapFile`が新しい識別子を発明する必要はありません。
`RecordId`はすでに「あるページの、あるスロット」を指す型として設計されていて、`HeapFile`はその`PageId`をどのページの`DiskManager::read_page`に渡すかを知っているだけです。

## insert、update、deleteの設計とRecordIdの扱い

`insert`は、既存のページを先頭から順に試し、`SlottedPage::insert`が入る場所を見つけられた最初のページへ書き込みます。

```rust
pub fn insert(&mut self, bytes: &[u8]) -> DbResult<RecordId> {
    for &page_id in &self.page_ids {
        let mut page = self.disk.read_page(page_id)?;
        if let Some(slot) = SlottedPage::open(page.payload_mut()).insert(bytes) {
            self.disk.write_page(&page)?;
            return Ok(RecordId::new(page_id, slot));
        }
    }

    let page_id = self.disk.allocate_page(PageType::Data)?;
    let mut page = self.disk.read_page(page_id)?;
    let slot = SlottedPage::init(page.payload_mut())
        .insert(bytes)
        .ok_or(DbError::TupleTooLarge(bytes.len()))?;
    self.disk.write_page(&page)?;
    self.page_ids.push(page_id);
    Ok(RecordId::new(page_id, slot))
}
```

`page_ids`の先頭から順に試すこの探し方は、テーブルのページ数が増えるほど、空きページを見つけるまでのコストもページ数に比例して増えていく線形探索です。
どのページにどれだけ空きがあるかを別に記録しておき、空きのあるページを直接指せるようにするFree Space Mapは、第15章で扱います。
この章の時点では、まだテーブルのページ数がその線形探索を問題にするほど多くなる場面を扱わないので、素朴な実装のままにしています。

どのページにも入らなければ、新しいページを1枚割り当てて`SlottedPage::init`し、そこへ挿入します。
新しいページに`init`した直後ですら`bytes`が入らない場合、それは`bytes`自体がページの`payload`に対して大きすぎることを意味するので、`DbError::TupleTooLarge`を返します。

`get`と`delete`は、`SlottedPage`の対応するメソッドをそのまま呼び出す薄い実装です。

```rust
pub fn get(&self, rid: RecordId) -> DbResult<Option<Vec<u8>>> {
    let mut page = self.disk.read_page(rid.page_id)?;
    Ok(SlottedPage::open(page.payload_mut())
        .get(rid.slot_id)
        .map(|bytes| bytes.to_vec()))
}
```

`update`だけは、単に`SlottedPage::update`を呼ぶだけでは済みません。

```rust
pub fn update(&mut self, rid: RecordId, bytes: &[u8]) -> DbResult<Option<RecordId>> {
    let mut page = self.disk.read_page(rid.page_id)?;

    let occupied =
        SlottedPage::open(page.payload_mut()).status(rid.slot_id) == Some(SlotStatus::Occupied);
    if !occupied {
        return Ok(None);
    }

    if SlottedPage::open(page.payload_mut()).update(rid.slot_id, bytes) {
        self.disk.write_page(&page)?;
        return Ok(Some(rid));
    }

    // このページの中には(コンパクションしても)収まらないので、
    // このページからは削除し、別のページへ挿入し直す。
    SlottedPage::open(page.payload_mut()).delete(rid.slot_id);
    self.disk.write_page(&page)?;
    let new_rid = self.insert(bytes)?;
    Ok(Some(new_rid))
}
```

第12章の`SlottedPage::update`は、1ページの中でコンパクションを試みてもなお新しいバイト列が収まらなければ`false`を返すだけで、そこから先の面倒は見ません。
`HeapFile::update`は、その`false`を受け取ったときに初めて動きます。
元のスロットを削除し、あらためて`self.insert(bytes)`を呼んで、空きのある(あるいは新しく確保する)別のページへ挿入し直します。

この移動が起きると、返される`RecordId`は元の`rid`とは別の値になります。
`HeapFile::update`は、`RecordId`を移動後も固定するための間接参照(たとえば「移動先を指すポインタを元の場所に残す」といった仕組み)を導入しないという設計を選んでいます。
1ページの中でどうにか収める`SlottedPage::update`の割り切りを、ページをまたぐケースにもそのまま延長したかたちです。
この設計のもとでは、`update`を呼び出す側は戻り値の`RecordId`を必ず以後のアクセスに使う必要があります。
呼び出し側が戻り値を無視して元の`rid`を使い続けると、ページをまたぐ更新のときにその`rid`はもう存在しないレコードを指すことになります。

最後に`scan`です。

```rust
pub fn scan(&self) -> Scan<'_> {
    Scan {
        disk: &self.disk,
        page_ids: self.page_ids.iter(),
        current: None,
    }
}
```

`Scan`は、今読み込んでいるページとその中の走査位置だけを保持するイテレータで、`next`が呼ばれるたびに1件ずつ生きているタプルを返します。

```rust
impl Iterator for Scan<'_> {
    type Item = DbResult<(RecordId, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some((page, slot_idx)) = self.current.as_mut() {
                let page_id = page.page_id;
                let slot_count = SlottedPage::open(page.payload_mut()).slot_count() as u16;
                while *slot_idx < slot_count {
                    let slot = SlotId(*slot_idx);
                    *slot_idx += 1;
                    if let Some(bytes) = SlottedPage::open(page.payload_mut()).get(slot) {
                        let rid = RecordId::new(page_id, slot);
                        return Some(Ok((rid, bytes.to_vec())));
                    }
                }
                self.current = None;
                continue;
            }

            let next_page_id = *self.page_ids.next()?;
            match self.disk.read_page(next_page_id) {
                Ok(page) => self.current = Some((page, 0)),
                Err(err) => return Some(Err(err)),
            }
        }
    }
}
```

ページ内の全スロットを見終えたら`self.current`を`None`に戻し、次のループで`page_ids`の次の要素を読み込みます。
`Item`が`DbResult<(RecordId, Vec<u8>)>`になっているのは、`disk.read_page`がI/Oエラーやchecksum不一致で失敗しうるからです。
ページ単位でしか先読みしないこの実装は、`HeapFile`全体の内容を一度にメモリへ読み込むことをしません。

## テストで確認する

`disk_manager`と`heap_file`のテストは、`tempfile`のような外部クレートを新たに依存に加えず、`std::env::temp_dir()`にプロセスIDと現在時刻を組み込んだ一意な名前を組み合わせて一時ファイルのパスを作っています。

```rust
fn temp_path(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    let unique = format!(
        "minidb-heap-file-test-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    path.push(unique);
    path
}
```

`DiskManager::open`は指定したパスを`create(true)`で開くので、この程度の一意性があれば十分に衝突を避けられます。
`tempfile`クレートは、シグナルやパニックを挟んでもファイルの削除を保証するような、この章のテストが必要としない機能まで持っています。
テストの最後で明示的に`std::fs::remove_file`を呼べば済む範囲であれば、新しい依存を増やす理由はありません。

`DiskManager`のテストでは、まず素朴なラウンドトリップを確認します。

```rust
#[test]
fn allocate_write_and_read_page_round_trips() {
    let path = temp_path("round-trip");
    let disk = DiskManager::open(&path).unwrap();

    let id = disk.allocate_page(PageType::Data).unwrap();
    assert_eq!(id, PageId(1));
    assert_eq!(disk.page_count(), 2);

    let mut page = disk.read_page(id).unwrap();
    page.payload_mut()[0..5].copy_from_slice(b"hello");
    disk.write_page(&page).unwrap();

    let reread = disk.read_page(id).unwrap();
    assert_eq!(&reread.payload()[0..5], b"hello");

    std::fs::remove_file(&path).unwrap();
}
```

次に、プロセス内で`DiskManager`を一度閉じてから同じパスをもう一度`open`し、書き込んだ内容が残っていることを確認します。

```rust
#[test]
fn reopening_the_same_file_preserves_pages() {
    let path = temp_path("reopen");
    {
        let disk = DiskManager::open(&path).unwrap();
        let id = disk.allocate_page(PageType::Data).unwrap();
        let mut page = disk.read_page(id).unwrap();
        page.payload_mut()[0..5].copy_from_slice(b"world");
        disk.write_page(&page).unwrap();
        disk.sync().unwrap();
    }

    let disk = DiskManager::open(&path).unwrap();
    assert_eq!(disk.page_count(), 2);
    let page = disk.read_page(PageId(1)).unwrap();
    assert_eq!(&page.payload()[0..5], b"world");

    std::fs::remove_file(&path).unwrap();
}
```

内側のブロックを抜けるところで`disk`がドロップされ、`File`が閉じます。
`write_page`のあとに明示的に`sync`を呼んでいるのは、この章で説明したとおり`write_page`自体はディスクへ届いたことまで保証しないからです。
この`sync`を省くと、環境によっては(OSのページキャッシュがまだ有効なうちにテストプロセスが読み直すだけなので)テスト自体は通ってしまいますが、それは`sync`の意味を確認したことにはなりません。

破損の検出は、正しく書き込んだファイルをテストの中から直接開き、1バイトだけ書き換えてから`DiskManager::open`をやり直すことで確認します。

```rust
#[test]
fn corrupting_a_byte_on_disk_is_detected_on_read() {
    let path = temp_path("corrupt");
    {
        let disk = DiskManager::open(&path).unwrap();
        let id = disk.allocate_page(PageType::Data).unwrap();
        let mut page = disk.read_page(id).unwrap();
        page.payload_mut()[0..5].copy_from_slice(b"alice");
        disk.write_page(&page).unwrap();
        disk.sync().unwrap();
    }

    // ファイルを直接開き、ページ1(2ページ目)の途中のバイトを1つ反転させる。
    {
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(PAGE_SIZE as u64 + 20)).unwrap();
        file.write_all(&[0xFF]).unwrap();
    }

    let disk = DiskManager::open(&path).unwrap();
    let err = disk.read_page(PageId(1)).unwrap_err();
    assert!(matches!(err, DbError::CorruptPage(_)));

    std::fs::remove_file(&path).unwrap();
}
```

書き換えているのはページ1の`payload`の5バイト目で、Page Headerからは離れた位置です。
それでも検出できるのは、第11章のchecksumがPage Headerと`payload`の両方をまとめて1回で保護しているからです。
Disk Managerの層では、その検証を自分で書き直さず、`Page::decode`が返すエラーをそのまま呼び出し元へ伝えるだけで、この検出が実現できています。

`heap_file`のテストでは、複数ページにまたがる挿入と走査を確認します。

```rust
#[test]
fn insert_across_multiple_pages_and_scan_returns_them_all() {
    let path = temp_path("multi-page-scan");
    let disk = DiskManager::open(&path).unwrap();
    let mut heap = HeapFile::open(disk);

    // 1ページに収まらない件数を入れ、複数ページへまたがらせる。
    let mut inserted = Vec::new();
    for i in 0..500u32 {
        let bytes = format!("row-{i:04}").into_bytes();
        let rid = heap.insert(&bytes).unwrap();
        inserted.push((rid, bytes));
    }

    assert!(heap.page_ids().len() > 1);

    let scanned: Vec<_> = heap.scan().collect::<DbResult<Vec<_>>>().unwrap();
    assert_eq!(scanned.len(), inserted.len());
    for (rid, bytes) in &inserted {
        assert!(scanned.contains(&(*rid, bytes.clone())));
    }

    std::fs::remove_file(&path).unwrap();
}
```

500件という件数自体に特別な意味はなく、1ページ(`payload`が4080バイト)には到底収まらず、`heap.page_ids().len() > 1`が確実に成り立つだけの数を選んでいます。
`DiskManager`と同じ形で、`HeapFile`を一度閉じてから開き直しても中身が残ることも確認しています。

```rust
#[test]
fn reopening_the_disk_manager_preserves_the_heap_file_contents() {
    let path = temp_path("reopen");
    let mut inserted = Vec::new();
    {
        let disk = DiskManager::open(&path).unwrap();
        let mut heap = HeapFile::open(disk);
        for i in 0..300u32 {
            let bytes = format!("row-{i:04}").into_bytes();
            let rid = heap.insert(&bytes).unwrap();
            inserted.push((rid, bytes));
        }
        // heapのDiskManagerはここでスコープを抜けてdropされる(closeに相当)。
    }

    let disk = DiskManager::open(&path).unwrap();
    let heap = HeapFile::open(disk);
    let scanned: Vec<_> = heap.scan().collect::<DbResult<Vec<_>>>().unwrap();
    assert_eq!(scanned.len(), inserted.len());
    for (rid, bytes) in &inserted {
        assert_eq!(heap.get(*rid).unwrap(), Some(bytes.clone()));
    }

    std::fs::remove_file(&path).unwrap();
}
```

`update`がページをまたいで`RecordId`を変える場面も、意図的に作って確認します。

```rust
#[test]
fn update_that_does_not_fit_moves_to_another_page_and_changes_the_record_id() {
    let path = temp_path("update-move");
    let disk = DiskManager::open(&path).unwrap();
    let mut heap = HeapFile::open(disk);

    // 1ページ目に2件を隙間なく詰める。どちらも生きているので、
    // 片方を削除してコンパクションしても、もう片方の分だけ空きは
    // 埋まったままになる。
    let a = vec![b'a'; 2000];
    let b = vec![b'b'; 2000];
    let _rid_a = heap.insert(&a).unwrap();
    let rid_b = heap.insert(&b).unwrap();

    // aが生きたまま残るページには、この大きさは(コンパクションしても)
    // 収まらないため、別ページへ移動するはずである。
    let bigger = vec![b'c'; 3000];
    let new_rid = heap.update(rid_b, &bigger).unwrap().unwrap();

    assert_ne!(new_rid.page_id, rid_b.page_id);
    assert_eq!(heap.get(rid_b).unwrap(), None);
    assert_eq!(heap.get(new_rid).unwrap(), Some(bigger));
    assert_eq!(heap.get(_rid_a).unwrap(), Some(a));

    std::fs::remove_file(&path).unwrap();
}
```

2000バイトずつの`a`と`b`を同じページに入れると、残りの空きは`b`を3000バイトへ置き換えるにはどう頑張っても足りません。
`a`が生きたまま残っている以上、`b`を削除してコンパクションしても空くのは`b`の分の2000バイトだけで、3000バイトの`bigger`はやはり収まらないからです。
このテストは、`new_rid.page_id`が元の`rid_b.page_id`と異なることに加え、元の`rid_b`ではもう`bigger`が読めず(`None`)、新しい`new_rid`で読めること、そして無関係だった`a`がそのページに残ったままであることも確認しています。

## 到達点

この章で作った`DiskManager`と`HeapFile`は、まだSQLの実行経路には接続していません。
`INSERT INTO users VALUES (1, 'Alice')`を実行しても、今までどおり第10章の`MemStorage`が使われ、この章のコードは一切呼ばれません。
`DiskManager::open`でファイルを開き、`HeapFile::open`でテーブルとして扱い、`insert`、`get`、`update`、`delete`、`scan`を呼ぶという一連の流れそのものは、この章のテストの中だけで完結しています。

それでも、ディスクに書いたバイト列がプロセスを再起動しても残っているという、第11章の冒頭で立てた目標は、この章でようやく実際に確認できるようになりました。
`FileHeader`と`Page`が定めたバイト列の形式、`SlottedPage`が定めたページ内のレコードの詰め方、そしてこの章の`DiskManager`が定めたページ番号とファイルオフセットの対応、この3つが組み合わさって、複数ページにまたがるテーブルをファイルとして持ち運べるようになりました。
第14章のBuffer Poolは、この`DiskManager`への読み書きのたびに実ファイルへアクセスするのではなく、よく使うページをメモリ上に留めておく仕組みを間に挟みます。
第15章では、この章で「1ファイル1テーブル」に割り切っていた制約を外し、複数のテーブルを1つのファイルに共存させるカタログと、線形探索に頼っていた空きページ探しを置き換えるFree Space Mapを導入します。

## 演習問題

### 必須課題

1. `DiskManager::write_page`を呼んだ直後に`sync`を呼ばなければ、そのページの内容がディスクへ実際に届いているかどうかは保証されません。この章の`reopening_the_same_file_preserves_pages`テストから`disk.sync().unwrap();`の行を削除して実行し、テストの結果がどうなるか観察してください。結果が変わらなかった場合、それが「`sync`が実は不要である」ことを意味しないのはなぜか、この章の説明を踏まえて考えてください。
2. `HeapFile::update`が新しいバイト列をページ内に収められず、別のページへ移動する分岐を読んでください。この移動が起きたとき、元の`rid`を使い続けているコードがあると何が起きるか、`update_that_does_not_fit_moves_to_another_page_and_changes_the_record_id`テストの`heap.get(rid_b)`の結果を根拠に説明してください。
3. `DiskManager::open`の`verify_existing_file`は、`FileHeader`のchecksumに加えて、実際のファイルサイズが`page_count * PAGE_SIZE`と一致するかどうかも確認しています。この検証を削除した場合、`corrupting_a_byte_on_disk_is_detected_on_read`のようなchecksum検出のテストには影響しないはずですが、どのような壊れ方に対する検出が失われるか、具体的なシナリオを1つ考えてください。

### 発展課題

1. `HeapFile::insert`は、空きのあるページを`page_ids`の先頭から線形探索します。テーブルが数百から数千ページに育った場合、この探索コストがどう効いてくるか、そしてどのような追加のデータ構造(この章の本文が触れているFree Space Mapのようなもの)があれば線形探索を避けられるかを、実装せずに設計だけ考えてください。
2. `DiskManager`は、複数のスレッドから`Arc<DiskManager>`として共有されることを見越して`&self`でページI/Oを提供していますが、内部の`Mutex<Inner>`は全てのページへのアクセスを1本のロックで直列化しています。異なるページへの`read_page`同士が互いを待たされない設計にするとしたら、`Inner`の持ち方をどう変える必要があるか考えてみてください。
3. `DiskManager::allocate_page`は、データページの書き込みとMetaページの書き直しという2回の書き込みを行います。この2回の書き込みの間でプロセスが強制終了した場合、次に`DiskManager::open`したときにファイルの状態がどうなるか(`page_count`と実際のページ数の関係を含めて)を考え、実際にテストコードで再現できるか試してください。
