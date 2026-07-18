# 第15章 永続カタログと空き領域管理

前章末の`reopening_the_disk_manager_preserves_the_heap_file_contents`テストを、もう一度見てみます。

```rust
let disk = DiskManager::open(&path).unwrap();
let heap = HeapFile::open(BufferPool::new(disk, 8));
let scanned: Vec<_> = heap.scan().collect::<DbResult<Vec<_>>>().unwrap();
```

`HeapFile::open`は`DiskManager`を渡されただけで、300件のタプルがどのページに散らばっているかを正しく復元します。
種を明かせば、これは`HeapFile::open`の中身を見ればすぐわかる、ほとんど力任せの方法で動いています。

```rust
pub fn open(pool: BufferPool) -> Self {
    let page_ids = (1..pool.page_count()).map(PageId).collect();
    HeapFile { pool, page_ids }
}
```

ページ0(Metaページ)を除く、ファイルにある全ページを問答無用でこのテーブルのデータページとみなしているだけです。
このテーブルがどのページを使っているかという情報は、ファイルのどこにも書かれていません。
`HeapFile::open`が毎回それらしく動いて見えるのは、「ファイルの残り全部がこのテーブルのものだ」という前提が、1ファイル1テーブルという第13章以来の制約のもとでたまたま正しいからにすぎません。

この前提は、2つ目のテーブルを同じファイルに置いた瞬間に崩れます。
`users`テーブルと`posts`テーブルを同じファイルに共存させたら、`HeapFile::open`は両方のページを区別なく1つのテーブルのものとして読み込んでしまい、`posts`の行が`users`のテーブルスキャンに紛れ込みます。
そもそも第9章の`Catalog`もこの章まで、テーブル定義をプロセスのメモリ上にしか持っていませんでした。
`CREATE TABLE users (...)`を実行した直後にプロセスを再起動すれば、`users`という名前とその列構成そのものが消えます。
ページの中身が(第13章のおかげで)ディスクに残っていても、そこに`users`という名前の表があったことを覚えている場所がどこにもなければ、二度とたどり着けません。

この章では、「どのテーブルが、どの名前で、どの列構成で、どのページを使っているか」というテーブル定義そのものを1つのファイルへ永続化し、複数のテーブルを1つのファイルに同居させます。
あわせて、`HeapFile::insert`が空きページを探すときに使っていた素朴な線形探索を、ページごとの空き容量の見積もりを持つ**Free Space Map**に置き換えます。

## カタログをどこに置くか

テーブル定義を書き出す場所を自由に選べるとしたら、どこに置くべきでしょうか。

素朴に考えると、「テーブルの一覧が書かれたページのIDを、どこかに記録しておく」という案が浮かびます。
しかしこの案は、記録する場所自体をどこに置くかという同じ問題を1つ先送りしただけです。
カタログの場所を知るためにはまず何かを読まなければならず、その「何か」もまたどこかに置かれていて、それがどこにあるかを知るには……という堂々巡りに入ります。
鶏が先か卵が先かというこの問題を解決した前例は、実はすでに第11章にあります。
File Headerは、ファイルの先頭という**固定位置**(ページ0)に置くと決めることで、「File Headerがどこにあるか」という問いそのものをコードの外に追い出しました。
`DiskManager::open`はページ0を読めば必ずFile Headerに行き着くと信じ切っていて、実際その信頼は裏切られません。

この章のカタログも同じ発想を採ります。
ページ0はすでにFile Headerが占有しているので、空いている次の番号、ページ1をカタログの定位置とします。
このページを、以降**Catalogページ**と呼びます。

```rust
/// Catalogページの定位置。ページ0はFile Header(第11章)が占有しているため、
/// 空いている最初の番号を使う。
const CATALOG_PAGE_ID: PageId = PageId(1);
```

Catalogページは、第11章の`PageType`に新しく加えた種類のページとして扱います。

```rust
pub enum PageType {
    /// File Header専用ページ。
    Meta,
    /// 一般のデータページ。
    Data,
    /// `Storage`(第15章)が使う、テーブル定義とFree Page Listを保持するページ。
    Catalog,
}
```

種類をただの整数(`u8`)ではなく`PageType`という列挙型で持たせておく判断は第11章のものですが、その効果はこの章になって初めて実感できます。
`Storage::open`は、ページ1を読み込んだら、その中身をカタログとして解釈する前に「これは本当にCatalogページか」を確認します。

```rust
let guard = pool.read_page(CATALOG_PAGE_ID)?;
if guard.page_type() != PageType::Catalog {
    return Err(DbError::CorruptPage(format!(
        "PageId({})はCatalogページである必要がありますが{:?}でした",
        CATALOG_PAGE_ID.0,
        guard.page_type()
    )));
}
```

`page_type()`は、この章のために`PageReadGuard`と`PageWriteGuard`(第14章)へ追加した小さなアクセサです。
ページの中身をバイト列として解釈する前に、そのページが名乗っている種類そのものを検査できるようにしておくと、たとえば将来のバグでデータページがページ1の位置に紛れ込んだような場合でも、カタログとして誤読する前に検出できます。

## Catalogページのレイアウト

Catalogページの`payload`には、次の内容を第11章と第12章から続く手書きのリトルエンディアンでエンコードします。

```text
next_table_id:    u64
table_count:      u32
free_page_count:  u32
free_page_ids:    u64 × free_page_count
tables × table_count:
    table_id:       u64
    name_len:       u16
    name:           u8 × name_len
    column_count:   u16
    columns × column_count:
        col_name_len: u16
        col_name:     u8 × col_name_len
        data_type:    u8 (0=BOOLEAN, 1=BIGINT, 2=TEXT)
        nullable:     u8 (0 または 1)
    page_count:     u32
    page_ids:       u64 × page_count
```

`next_table_id`を先頭に置いているのは偶然ではありません。
第9章の`Catalog`は、`TableId`を単調増加で払い出し、`DROP TABLE`で番号が空いても再利用しないという方針を持っていました。

```rust
let id = TableId(self.next_table_id);
self.next_table_id += 1;
```

この`next_table_id`がメモリ上にしかなければ、`users`を作って削除して再起動するだけで、次に`CREATE TABLE`したテーブルが再びIDを`0`から始めてしまい、「削除後もIDは再利用しない」という第9章の不変条件が再起動のたびに崩れます。
`next_table_id`をカタログの一部として永続化するのは、この不変条件を再起動の前後で守り続けるためです。

各テーブルのエントリは、名前、列構成に続けて`page_ids`(そのテーブルが使っているページ番号の一覧)を持ちます。
ここが、この章のレイアウトのうちもっとも成長しやすい部分です。
ページ番号1つが8バイトなので、テーブルが抱えるページ数が増えるほど、そのテーブルのカタログ上の専有量も線形に増えていきます。
Catalogページ1枚のバイト数(`PAGE_PAYLOAD_SIZE`、4080バイト)には当然上限があり、テーブルが十分に大きく育てば、いずれこの上限を超えます。
この章では、超えた場合を複数ページへの分割では解決せず、`DbError::CatalogTooLarge`を返すという単純な割り切りにとどめます。

```rust
fn persist_catalog(&self) -> DbResult<()> {
    let bytes = encode_catalog(self.next_table_id, &self.tables, &self.free_pages);
    if bytes.len() > PAGE_PAYLOAD_SIZE {
        return Err(DbError::CatalogTooLarge(bytes.len(), PAGE_PAYLOAD_SIZE));
    }
    let mut guard = self.pool.write_page(CATALOG_PAGE_ID)?;
    let data = guard.data_mut();
    data[..bytes.len()].copy_from_slice(&bytes);
    data[bytes.len()..].fill(0);
    Ok(())
}
```

テーブルごとに専用のカタログエントリページを持たせるといった、カタログ自体を複数ページにまたがらせる構成は、この章では扱いません。
章末の演習で考えます。

エンコードする際、テーブルは`TableId`の昇順で書き出します。

```rust
let mut sorted: Vec<(&TableId, &TableEntry)> = tables.iter().collect();
sorted.sort_by_key(|(id, _)| id.0);
```

`tables`は`HashMap`なので、反復順は実行のたびに変わりえます。
書き出す順序を固定しておかないと、論理的にはまったく同じカタログの状態でも、エンコードした結果のバイト列が実行ごとに異なってしまいます。
ここでの並べ替えは、その場しのぎの対症療法ではなく、「カタログの内容が同じなら、ディスク上のバイト列も同じであってほしい」という素朴な要求を満たすための、最初から織り込むべき設計です。

デコード側(`decode_catalog`)は、第4章の`tuple_codec`と同じ手作業のカーソルで書きます。
このカーソルには1つだけ、`tuple_codec`より注意が必要な点があります。
`table_count`、`column_count`、`page_count`のような「これから何個読むか」を宣言する値は、バイト列が壊れていれば根拠のない数字になりえます。

```rust
fn decode_catalog(bytes: &[u8]) -> DbResult<DecodedCatalog> {
    let mut cursor = bytes;
    let next_table_id = take_u64(&mut cursor, "next_table_id")?;
    let table_count = take_u32(&mut cursor, "table_count")? as usize;
    // ...
    let mut tables = HashMap::new();
    for _ in 0..table_count {
        // ...
    }
```

もし`table_count`をそのまま`Vec::with_capacity(table_count)`のように使ってしまうと、壊れたバイト列に埋め込まれた巨大な数字1つで、実際のデータ量とは無関係に大きなメモリ確保が起きてしまいます。
この章の`decode_catalog`は、宣言された個数を確保のヒントには使わず、`Vec::new()`から`push`で積み上げていく素朴な形にとどめています。
各要素を読む前には必ず`take`が残りバイト数を検査するため、壊れたバイト列を渡されたときに実際に確保されるメモリ量は、そのバイト列自身の長さ(高々`PAGE_PAYLOAD_SIZE`)で頭打ちになります。

```rust
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
```

## 削除されたページを使い回すFree Page List

`DROP TABLE`されたテーブルが使っていたページは、ファイルからは消えません。
`DiskManager`には、確保したページをファイルから手放す(ファイルを縮める)手段がそもそも無いからです。
この章では、そのページを**Free Page List**という一覧に積んでおき、次にどれかのテーブルが新しいページを必要としたとき、ファイルを伸ばすより先にそこから1枚もらうという方法で再利用します。

```rust
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
```

`drop_table`の時点では、ページの中身自体は書き換えません。
削除されたテーブルの古いタプルは、Free Page Listに積まれた後もそのページに残ったままです。
中身を作り直すのは、そのページが実際に再利用されるときです。

```rust
fn try_insert_into_fresh_page(
    &mut self,
    page_id: PageId,
    bytes: &[u8],
) -> DbResult<Option<RecordId>> {
    let mut guard = self.pool.write_page(page_id)?;
    let slot = SlottedPage::init(guard.data_mut()).insert(bytes);
    // ...
}
```

`SlottedPage::init`(第12章)は、スロット0件、空き領域は全体という初期状態でページを上書きします。
Free Page Listから取り出したページにも、`BufferPool::allocate_page`で確保したばかりの新しいページにも、この同じ関数を使えるのはこのためです。
以前どのテーブルの、どんな中身のタプルが入っていたかは、再利用の瞬間に一切引き継がれません。

## 空きページを探すFree Space Map

`HeapFile::insert`(第13章)は、空きのあるページを`page_ids`の先頭から順に試す線形探索でした。

```rust
for &page_id in &self.page_ids {
    let mut guard = self.pool.write_page(page_id)?;
    if let Some(slot) = SlottedPage::open(guard.data_mut())?.insert(bytes) {
        return Ok(RecordId::new(page_id, slot));
    }
}
```

この「線形探索」という言葉が指しているコストは、実はページ数そのものではありません。
`pool.write_page(page_id)?`の1回1回が、そのページを`BufferPool`からpinし、`Mutex`のロックを取り、`SlottedPage`として開いて`insert`を試みる、という一式の重い手続きです。
すでに満杯だとわかっているページに対しても、そのたびにこの手続きをまるごと払っています。
テーブルが数百ページに育ち、そのほとんどが埋まっている状態でも、`insert`は毎回この重い手続きを先頭から繰り返すことになります。

この章の**Free Space Map**は、「このページには残りおよそ何バイトの空きがあるか」という整数1つだけをメモリ上に持ち、`BufferPool`にもディスクにも触れずに候補を絞り込めるようにします。

```rust
pub fn find_candidate(&self, candidates: &[PageId], needed: usize) -> Option<PageId> {
    candidates
        .iter()
        .copied()
        .find(|page_id| self.free_bytes.get(page_id).is_some_and(|&free| free as usize >= needed))
}
```

ここで正直に書いておくべきことがあります。
この`find_candidate`は、依然として`candidates`(テーブルが持つページの一覧)の長さに比例する時間がかかります。
変わっていないのは「何ページぶん調べるか」という漸近的な計算量で、変わったのは「1候補あたり何をするか」です。
`HeapFile`の線形探索は1候補ごとに`BufferPool`のpin、ロック、Slot Directoryの走査という一式を払いましたが、Free Space Mapの探索は1候補ごとにメモリ上のハッシュ表参照1回で済みます。
満杯だとわかっているページを、実際にpinして開いてから気付くのではなく、その手前で弾けるようになったというのが、この章の改善の中身です。

粒度と更新のタイミングも決めておく必要があります。
この章のFree Space Mapは、`SlottedPage::free_space`が返すバイト数をそのまま保持します。

```rust
pub fn update(&mut self, page_id: PageId, free_bytes: usize) {
    self.free_bytes
        .insert(page_id, free_bytes.min(u16::MAX as usize) as u16);
}
```

PostgreSQLのFree Space Mapは、1バイトを256段階に量子化した近似値を使っています。
これは、ページ数もタプルの出入りも桁違いに多い環境で、更新の頻度とファイルサイズの両方を抑えるための最適化です。
この章の`minidb`が扱うページ数はその最適化を要するほど大きくないため、量子化はせず実測値をそのまま持つ単純な実装を選びました。
更新のタイミングは、`insert`、`update`、`delete`がページを書き換えた直後です。
そのとき開いていた`SlottedPage`から`free_space()`を読み直すだけで最新値が手に入るので、追加のページ読み込みは要りません。

```rust
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
```

Free Space Map自体はディスクへ永続化しません。
ページの中身こそが空き容量の一次情報であり、Free Space Mapはその要約をメモリ上に持つキャッシュにすぎないからです。
`Storage::open`は、カタログから復元した各テーブルの`page_ids`を1回ずつ読み、実際の`free_space()`からFree Space Mapを作り直します。
起動のたびに実測から作り直すほうが、更新のたびにディスク上の値とメモリ上の値という2箇所を同期させ続けるより単純です。

この設計には見落としやすい弱点が1つあります。
`SlottedPage::free_space`が返すのは、Slot DirectoryとTuple Dataの間にまだ手つかずで残っている隙間だけです。
Tombstone化された(削除済みの)タプルが専有したままの領域は、`compact`(第12章)を呼ぶまで空き領域に数えられません。
Free Space Mapはこの値をそのまま保持するだけなので、あるページで大量のタプルを削除しても、`compact`が実際に呼ばれるまでは空きが増えたとは見なしません。
`insert`はそのページを候補から外し、代わりに別のページか新しいページへ進みます。
この見落としはデータの正しさには影響しません。
除外されたページは単に候補から外れるだけで、挿入自体は別の場所へ正しく行われます。
影響するのはページの利用効率で、削除によって本来なら収まったはずの領域が、しばらく死んだままになりえます。
コンパクション後の空きまで見積もりに含める改良は、章末の演習に譲ります。

## Storageエンジンの公開API

ここまでの3つの部品(Catalogページ、Free Page List、Free Space Map)を組み合わせ、`Storage`という1つの入口にまとめます。

```rust
pub struct Storage {
    pool: BufferPool,
    next_table_id: u64,
    tables: HashMap<TableId, TableEntry>,
    /// `DROP TABLE`によって空いた、再利用待ちのページの一覧。
    free_pages: Vec<PageId>,
    fsm: FreeSpaceMap,
}
```

`create`と`open`は、第13章の`DiskManager::open`が担っていた「新規作成か、既存ファイルを開くか」という区別を、Storageのレイヤーでも繰り返します。

```rust
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
```

`create`は、渡されたパスがまだFile Headerしか持たない(`page_count == 1`)ときだけ初期化を進めます。
すでにCatalogページを持つファイルに対して`create`を呼んだら、それは既存のデータベースを壊して作り直そうとしている可能性が高いので、エラーで拒みます。
既存のファイルを正しく読み込みたいときは`open`を使います。

```rust
pub fn open<P: AsRef<Path>>(path: P) -> DbResult<Self> {
    let disk = DiskManager::open(path)?;
    if disk.page_count() < 2 {
        return Err(DbError::CorruptPage(
            "Catalogページがありません(Storage::createで作成したファイルではありません)"
                .to_string(),
        ));
    }
    // ...
}
```

`DiskManager::open`の内部で、File HeaderのMagic Number、Format Version、checksumはすでに検証されています(第13章)。
`Storage::open`はその結果を`?`でそのまま受け取ったうえで、Catalogページの有無とその`page_type`という、この章で新しく増えた前提をさらに確認します。

`create_table`、`drop_table`、`insert`、`get`、`update`、`delete`、`scan`という公開関数の形は、第9章の`Catalog`と第13章の`HeapFile`をそのまま足し合わせたような見た目をしています。

```rust
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
```

`self.next_table_id += 1`ではなく`checked_add(1)`を使っているのは、`next_table_id`が`u64::MAX`のときに素朴な加算だとオーバーフローするからです。
debugビルドではpanicし、releaseビルドでは`0`へ巻き戻って`TableId`の一意性が壊れます。
`Storage::open`が`next_table_id == u64::MAX`のカタログをすでに`DbError::CorruptCatalog`として拒む(次の節を参照)ため、通常この分岐に到達するのは`u64::MAX`回`create_table`を呼び続けた場合に限られますが、その防御をすり抜けてメモリ上だけで`next_table_id`が`u64::MAX`に達した場合の二重の備えとして`checked_add`を使っています。

`TableInfo`は第9章の`Catalog`がすでに持っていた型(`id`、`name`、`schema`)をそのまま再利用しています。
`insert`、`get`、`update`、`delete`は`TableId`を受け取り、名前からの解決は呼び出し側(次章で`Database`が担う想定)に任せる作りにしてあります。
これは`HeapFile`の`RecordId`が「ページとスロットの組」という下位のアドレスだけを扱っていたのと同じ立場で、`Storage`にとっての`TableId`も「どのテーブルか」を指すアドレスの1つにすぎません。

`insert`だけは、この章の3つの部品を実際に使い分ける場所なので、少し詳しく見ておきます。

```rust
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
                // 上の事前検査により、通常はここに到達しない。万一到達しても、
                // 取り出したページをFree Page Listへ戻しておく。
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
```

探索の前に、`bytes`が空の1ページにも収まらないほど大きくないかを`max_len_for_fresh_page`(第12章)で確認します。
この事前検査が無いと、失敗するだけの`insert`を何度呼んでも、そのたびにFree Page Listからページを1枚取り出したきり戻さない、あるいは`allocate_page`でファイルを1ページ伸ばしてしまいます。
同じ大きすぎる値を繰り返し`insert`しようとするコードがあれば、ファイルサイズが際限なく肥大化するということです。
この検査を最初に置いたことで、後続の3段階のどれにも進まないうちに`DbError::TupleTooLarge`を返せます。

探索そのものは3段階です。
まずFree Space Mapに、このテーブルの持ちページの中から空きの見積もりが十分なものを教えてもらいます。
見つからなければ、Free Page Listに再利用待ちのページがあればそれをもらいます。
それも無ければ、`BufferPool::allocate_page`でファイルへ新しいページを1枚追加します。

後半の2つの経路は、テーブルの`page_ids`を変えるためカタログを永続化し直します。
その永続化に失敗する場合(たとえば`DbError::CatalogTooLarge`)、`attach_page_to_table`は`page_ids`への追加を取り消します。
Free Page Listから取り出したページの場合は、それに加えて`insert`自身がそのページ番号を`free_pages`へ押し戻します。
そうしないと、せっかく再利用できたはずのページが、`page_ids`にも`free_pages`にもどこにも属さないまま宙に浮いてしまいます。

```rust
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
```

ここで正直に書いておきたい割り切りがもう1つあります。
`page_ids`への追加こそ取り消しますが、その直前にタプル自体はすでにそのページへ書き込まれてしまっています。
割り当てたページ自体も巻き戻しません。
つまりカタログの永続化に失敗すると、どのテーブルにも属さない、書き込み済みだが誰からも参照されないページがファイルに残ります。
コミットするかどうかを1つの操作として原子的に扱う仕組み(WAL)がまだ無いこの章では、この程度の取りこぼしを正直に残しておくほうが、無いはずの保証をあるかのように書くよりも誠実です。
この巻き戻しは、第33章のWrite-Ahead Loggingで初めてきちんと扱えるようになります。

## 壊して確認する

ここまでの設計が実際に効いているかどうかは、正常系のテストだけでは見えてきません。
壊れたファイルを`Storage::open`に渡してみることで、どの層がどの壊れ方を捕まえるのかを確かめます。

1つ目は、Catalogページのバイト列そのものを1バイト反転させる実験です。

```rust
#[test]
fn corrupting_a_byte_in_the_catalog_page_is_detected_on_open() {
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
```

このバイト反転は、Catalogページの`payload`のどこか1バイトを直接壊します。
第11章の`Page::decode`は、ページ全体に対するchecksumを検証してから中身を返す約束になっているので、`decode_catalog`が呼ばれる手前、`Page::decode`自身の時点でもう食い止められます。
`Storage`はカタログの解釈に一切手を付けていないのに、下の層(Page)の約束だけでこの壊れ方を検出できています。

2つ目は、もっと意地の悪い壊し方です。
checksumはわざと正しく計算し直し、その代わり中身の`name_len`(テーブル名の長さ)を、実際にページへ残っているバイト数よりずっと大きい値に書き換えます。

```rust
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
```

こうして作ったページは、`Page::decode`のchecksum検証を涼しい顔で通過します。
バイト列としては何も壊れていないからです。
壊れているのはその中身の**意味**であり、これを捕まえるのは`decode_catalog`の境界検査(`take`)の役目になります。

```rust
let err = expect_err(Storage::open(&path));
assert!(matches!(err, DbError::CorruptCatalog(_)));
```

`name_len`として宣言された値ぶんのバイト数がもう残っていないので、`take`がその場で`DbError::CorruptCatalog`を返します。

3つ目は、`decode_catalog`自身は正しく読み終えるのに、読み終えた**中身**が意味をなさない壊し方です。
`free_pages`(Free Page List)に、Metaページ(`PageId(0)`)を紛れ込ませたカタログを直接書き込んでみます。

```rust
let tables = single_table_entry(TableId(0), "a", Vec::new());
let bytes = encode_catalog(1, &tables, &[PageId(0)]);
write_catalog_payload(&path, &bytes);

let err = expect_err(Storage::open(&path));
assert!(matches!(err, DbError::CorruptCatalog(_)));
```

このバイト列は、`next_table_id`、`table_count`、`free_page_count`、各フィールドの宣言と実際の残りバイト数がすべて矛盾なく揃っており、`decode_catalog`の境界検査は何も引っかかりません。
それでも意味はおかしいままです。
`PageId(0)`は File Header(第11章)が占有しているMetaページであり、`Storage`が「空いていて再利用してよい」ページの一覧に加えてよい番号ではありません。
この矛盾を素通りさせると、次の`insert`が`free_pages`からこの番号を取り出し、`SlottedPage::init`でMetaページの中身をまるごと上書きしてしまいます。
そうなったファイルは、以後`DiskManager::open`のMagic Number検証にすら通らなくなり、二度と開けません。

`Storage::open`は、`decode_catalog`が返した状態を`fsm`へ組み立てる前に、この種の矛盾をまとめて検証します。
確認するのは、`next_table_id`が`u64::MAX`ではないこと(`u64::MAX`のままだと、次の`create_table`が`self.next_table_id`への加算でオーバーフローします)、参照している`PageId`が実際のページ数の範囲内にあること、Meta(`PageId(0)`)とCatalog(`PageId(1)`)という予約ページを指していないこと、あるページが複数のテーブル(または`free_pages`)に同時に属していないこと、そのページが実際に`PageType::Data`であること、テーブル定義の側では`TableId`とテーブル名が重複しておらず`next_table_id`より小さいこと、の6点です。
1つ目の実験がバイト列そのものの整合性(checksum)を、2つ目の実験がバイト列の**構造**の整合性(宣言された長さと実際の残りバイト数の対応)を、3つ目の実験がバイト列の**意味**の整合性(参照しているページとテーブル定義が矛盾なく成り立つこと)を、それぞれ別の層で検証しているとわかります。
Catalogページというたった1枚のページの安全性は、この3段の検査が揃って初めて成り立っています。

## 演習問題

### 必須課題

1. `FreeSpaceMap::find_candidate`は、削除によってページの空きが実際には増えていても、`compact`が呼ばれるまではその増加分を見積もりに反映しません。この章の`insert_skips_full_pages_and_lands_on_a_page_with_real_room`テストを土台に、「削除しても`insert`が同じページへ戻ってこない」ことを確認するテストを実際に書いてみてください。
2. `Storage::create_table`は、`persist_catalog`が失敗した場合にメモリ上の`tables`と`next_table_id`をロールバックします。もしこのロールバックを行わなかったら、次にどんな操作をしたときにどのような不整合が観測されるか、具体的な呼び出し順序を1つ構成して説明してください。
3. `decode_catalog`が個数(`table_count`、`column_count`、`page_count`)を`Vec::with_capacity`の引数に直接使わない設計になっている理由を、壊れたバイト列を渡した場合に実際に確保されるメモリ量という観点から説明してください。

### 発展課題

1. この章の`FreeSpaceMap`は、削除で回収できる空き(コンパクション後の空き)を見積もりに反映しません。`SlottedPage`または`SlottedPageRef`に、Tombstone化された領域の合計バイト数を返すメソッドを追加し、`FreeSpaceMap`の見積もりに「直接の空き」と「コンパクションすれば回収できる空き」を両方持たせる設計を考えてみてください。この見積もりをどちらの値で更新するかによって、`insert`の挙動(コンパクションが実際に起きる頻度)がどう変わるか説明してください。
2. この章のCatalogページは、`page_ids`をテーブルごとにそのまま書き出すため、ページ数が数百枚を超える巨大なテーブルでは`DbError::CatalogTooLarge`に達します。カタログを複数ページに分割する設計(たとえばテーブルごとに専用の「Table Metadataページ」を持たせ、Catalogページにはそのページ番号の一覧だけを書く)を考え、この章のレイアウトからの移行に何が必要か整理してください。
3. `Storage::insert`は、カタログの永続化に失敗した場合でも、直前に確保したページと書き込んだタプルを巻き戻しません。この「書き込み済みだがどのテーブルにも属さないページ」を検出し、再利用可能にする(Free Page Listへ回収する)ための`Storage`のメソッドを設計してみてください。ヒント: `tables`に登録されている全`page_ids`と、`free_pages`のどちらにも含まれないページを、ファイルの`page_count`から特定できます。
