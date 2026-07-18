# 第11章 データベースファイルとページ

第1部の最後、`minidb`は`CREATE TABLE`、`INSERT`、`SELECT`、`UPDATE`、`DELETE`を一通りこなせるようになりました。

```console
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> INSERT INTO users VALUES (1, 'Alice');
INSERT 1
minidb> SELECT * FROM users;
id | name
---------
1 | Alice
(1 row)
```

ここで`minidb`を終了し、もう一度起動してから同じ`SELECT`を打つとどうなるでしょうか。

```console
minidb> \q
$ cargo run --quiet
minidb> SELECT * FROM users;
エラー: テーブルが存在しません: users
```

`users`というテーブルごと消えています。
第9章の`Catalog`も第10章の`MemStorage`も、`Database::memory()`が`main`関数の中で確保した`Vec`や`HashMap`の上に載っているだけの構造体で、プロセスが終了すればその領域ごとOSに返却されます。
プロセスを起動し直すということは、空の`HashMap`から作り直すということであり、直前まで入っていた`Alice`という行を覚えている場所はどこにもありません。

この章から始まる第2部では、この記憶をディスクに移します。
ゴールは、`minidb`を終了して起動し直しても、`users`もその中の`Alice`も残っている状態です。
そこへ向かう最初の一歩として、この章ではディスク上のファイルを1バイト単位でどう組み立てるかを決めます。
Slotted Pageとしての行の詰め方(第12章)、ページ単位の読み書きを行うDisk Manager(第13章)、ページをメモリにキャッシュするBuffer Pool(第14章)は、いずれもこの章で決めるファイル形式の上に積み上がっていきます。

## なぜページという単位で管理するのか

ディスクに`Alice`という行を書く、と一口に言っても、書き方には幅があります。
最も素朴な案は、`INSERT`のたびにファイルの末尾へ行のバイト列をそのまま追記していくというものです。
この案でも、起動し直したときにファイルの先頭から読み直せば`Alice`は復元できます。

しかし、この素朴な案には`DELETE`や`UPDATE`が絡んだ瞬間に問題が生じます。
ファイルの途中にある1行を書き換えるには、その行より後ろにある全バイトを、新しい行の長さに合わせてずらして書き直す必要があります。
1行の削除や更新のたびにファイル全体を書き直すのでは、テーブルの行数に比例して1回の操作のコストが増え続けてしまいます。

OSのファイルI/Oも、この素朴な案とは相性がよくありません。
ディスクへの読み書きは、OSやディスク自身のキャッシュ機構によって、1バイトではなく決まった大きさの塊(多くは4KiBの倍数)を単位に行われます。
任意の位置に任意の長さのバイト列を挿入したり削除したりする操作は、この単位と噛み合わず、実際に書き込む必要のあるバイト数よりずっと広い範囲を巻き込んでしまいがちです。

そこでこの章からは、ファイルを**ページ**という固定長の区画に分割して管理します。
ページは、ファイル中のどこにあっても常に同じバイト数を持つ区画です。
1件の行の増減は、その行が属する1ページの中だけで完結させ、ページの外にあるバイト列には触れません。
ページという単位を固定してしまえば、「`N`番目のページはファイルの先頭から`N × PAGE_SIZE`バイト目にある」という単純な計算だけで、任意のページの位置をいつでも求められます。
この計算のしやすさは、第13章のDisk Managerが特定のページだけを読み書きするときの土台になり、第14章のBuffer Poolがページ単位でメモリにキャッシュする際の単位にもなります。

固定長という制約は、ページの中身がどんなに小さくても、ページ全体のバイト数を必ず使い切ることを意味します。
1行しか持たないページも、目一杯詰まったページと同じだけのディスク容量を消費します。
この余白は無駄ですが、その無駄と引き換えに、どのページも常に同じ位置計算、同じ読み書きの手順で扱えるという単純さを手に入れます。
ページの中身をどう詰めるか(第12章のSlotted Page)は、この余白をなるべく有効に使うための工夫です。

## ページのサイズをいくつにするか

固定長のページを採用すると決めたら、次はその長さを決めなければなりません。
この章では**4096バイト(4KiB)**をページサイズに選びます。

```rust
pub const PAGE_SIZE: usize = 4096;
```

4096バイトという数字は、多くの環境でOSの仮想メモリページのサイズそのものであり、ファイルシステムのI/Oブロックサイズもこの倍数であることがほとんどです。
ページサイズをこれらの単位に合わせておくと、1ページの読み書きが複数のOSページやディスクブロックにまたがりにくくなり、「1ページを書いたつもりが、隣接するページの一部まで巻き込んで書き換わってしまう」という部分書き込みの起きる余地が減ります。
SQLiteの既定のページサイズも4096バイトであり、実運用で広く検証されてきた値という意味でも妥当な選択です。

ページサイズを大きくする(たとえば8192バイトや16384バイト)という選択肢もありえます。
大きなページは1回のI/Oでより多くの行をまとめて読み書きできる一方、1ページ分の変更を丸ごと書き直すコストや、後の章で導入するWALがページ全体を保護する範囲も、その分だけ大きくなります。
この章の時点では、どちらのトレードオフが有利かを判断する材料(実際のワークロード、後の章で測るI/O性能)がまだありません。
判断材料の乏しい段階で大きい値を選ぶ理由もないため、素直な既定値である4096バイトから始めます。

## なぜ`serde`や`bincode`に頼らないのか

Rustでバイト列と構造体を相互変換するだけなら、`serde`で構造体に`#[derive(Serialize, Deserialize)]`を付け、`bincode`のようなバイナリフォーマットに投げるのが最短です。
この章では、あえてその最短路を採らず、`u32::to_le_bytes`や`u64::from_le_bytes`といった標準ライブラリの関数を使い、各フィールドがバイト列のどの位置に何バイトで置かれるかを1バイト単位で自分の手で書きます。

理由は、この章の主題そのものが「ファイルの中に何が何バイト目に置かれているか」だからです。
`serde`や`bincode`に変換を任せると、その配置は依存クレートの実装詳細の内側に隠れてしまい、`File Header`のどこにMagic Numberがあり、どこにchecksumがあるかを、コードを読むだけでは追えなくなります。
この章より後、第13章でDisk Managerがファイルの特定のオフセットだけを読み書きするようになったとき、そのオフセット計算がどこから来ているかを説明できるようにしておく必要があります。
バイト配置を自分で決め、自分でコードに書き下していれば、その説明はコードを読むだけで済みます。

もう1つの理由は、バイト順(**Byte Order**)の選択です。
複数バイトにまたがる整数をバイト列に変換するとき、上位バイトから並べる(**ビッグエンディアン**)か、下位バイトから並べる(**リトルエンディアン**)かは、実装が明示的に決めなければならない選択です。
`serde`を素朴に使うと、この選択はホストマシンのエンディアン(x86やARMの多くは通常リトルエンディアン)に暗黙に依存しがちで、異なるアーキテクチャで書き出したファイルを読み込むときに問題が起きえます。
この章では、`to_le_bytes`/`from_le_bytes`という関数名が示すとおり、**リトルエンディアンで固定**という決定を明示し、ファイルのどのフィールドも常にこの順序で読み書きします。

## File HeaderとPage Headerを設計する

ページという固定長区画に加えて、ファイル全体としてもう1つ管理すべき情報があります。
「このファイルは本当に`minidb`が作ったファイルなのか」「このファイルは何ページ分のデータを持っているのか」といった、個々のページの中身ではなくファイル全体に関わる情報です。
この情報を持つ場所を**File Header**と呼び、ファイルの先頭に固定長で置きます。

```rust
pub struct FileHeader {
    pub page_size: u32,
    pub page_count: u64,
}
```

`page_size`と`page_count`だけを構造体のフィールドに残し、Magic NumberとFormat Versionは定数として持たせています。
`FileHeader`が保持しているのは「このファイル固有の値」だけであり、Magic Numberは全ての`minidb`ファイルに共通の固定値、Format Versionはこの章の`minidb`のバージョンが対応している唯一の値だからです。
これらはバイト列に変換するときに書き込みますが、`FileHeader`自身のフィールドとして持ち回る必要はありません。

```rust
pub const MAGIC: [u8; 4] = *b"MDB1";
pub const FORMAT_VERSION: u32 = 1;
```

Magic Numberは、ファイルの先頭4バイトに置く固定の目印です。
`minidb`が何の変哲もないバイナリファイル(あるいは別のプログラムが作ったファイル)を誤って開こうとしたとき、この4バイトが一致しなければ、その場で「これは`minidb`のファイルではない」と判定できます。
Format Versionは、この章で決めるファイル形式そのものに付けるバージョン番号です。
将来ページの詰め方やヘッダーのレイアウトを変更したときにこの値を上げれば、古い形式のファイルを新しいコードで誤って読み、意味の取り違えたバイト列をそのまま構造体として解釈してしまう事故を防げます。

`encode`は、この`FileHeader`を24バイトの固定長バイト列に変換します。

```rust
pub fn encode(&self) -> [u8; FILE_HEADER_SIZE] {
    let mut buf = [0u8; FILE_HEADER_SIZE];
    buf[0..4].copy_from_slice(&MAGIC);
    buf[4..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf[8..12].copy_from_slice(&self.page_size.to_le_bytes());
    buf[12..20].copy_from_slice(&self.page_count.to_le_bytes());
    let checksum = crc32(&buf[0..20]);
    buf[20..24].copy_from_slice(&checksum.to_le_bytes());
    buf
}
```

`FILE_HEADER_SIZE`は`24`で、内訳はMagic Number(4バイト)、Format Version(4バイト)、`page_size`(4バイト)、`page_count`(8バイト)、checksum(4バイト)の合計です。
それぞれのフィールドがバイト列のどこに何バイトで置かれるかは、この`encode`の中の`copy_from_slice`の呼び出し順そのものが仕様になっています。

| オフセット | バイト数 | フィールド | 内容 |
| --- | --- | --- | --- |
| 0 | 4 | `magic` | 固定値`b"MDB1"` |
| 4 | 4 | `format_version` | 固定値`1`(リトルエンディアン) |
| 8 | 4 | `page_size` | このファイルの1ページあたりのバイト数(リトルエンディアン) |
| 12 | 8 | `page_count` | このファイルが持つページの総数(リトルエンディアン) |
| 20 | 4 | `checksum` | オフセット0〜19に対する`crc32`(リトルエンディアン) |

もう1つ、ページそれぞれの先頭にも固定長のヘッダーを置きます。
こちらは**Page Header**と呼び、File Headerとは別の情報を持ちます。

```rust
pub struct Page {
    pub page_id: PageId,
    pub page_type: PageType,
    payload: Vec<u8>,
}
```

`Page`は、`PAGE_SIZE`バイトのページを、Rust側では「先頭16バイトのPage Header相当の情報」と「残りの`payload`」に分けて保持する構造体です。
`page_id`は、このページがファイル中の何番目のページかを表す識別子で、第3章から骨格として存在していた`PageId`をここで初めて使います。
`page_type`は、このページが何を表すページかを区別するための値です。

```rust
pub enum PageType {
    Meta,
    Data,
}
```

この章で用意する種類は`Meta`(File Header専用のページ)と`Data`(一般のデータページ)の2つだけです。
B+Treeの内部ページと葉ページ(第23章)のように、後の章で新しい種類のページが必要になれば、この列挙型に足していきます。
先に全ての可能性を見越した種類を用意せず、必要になった時点で1つずつ増やしていくのは、第9章の`Catalog`や第10章の`executor`と同じ育て方です。

`payload`は`page_id`や`page_type`のような固定の意味を持たず、単なるバイト列として保持します。
このバイト列を「スロットの並び」として解釈するSlotted Pageの構造(第12章)は、まだこの章には登場しません。

`Page::encode`は、`Page`を`PAGE_SIZE`バイトのバイト列に変換します。

```rust
pub fn encode(&self) -> [u8; PAGE_SIZE] {
    let mut buf = [0u8; PAGE_SIZE];
    buf[0..8].copy_from_slice(&self.page_id.0.to_le_bytes());
    buf[8] = self.page_type.to_u8();
    // buf[9..12]は予約領域で、初期化済みの0のままにする。
    // buf[12..16]は次のcrc32計算までchecksum用に0を保つ。
    buf[PAGE_HEADER_SIZE..].copy_from_slice(&self.payload);
    let checksum = crc32(&buf);
    buf[12..16].copy_from_slice(&checksum.to_le_bytes());
    buf
}
```

| オフセット | バイト数 | フィールド | 内容 |
| --- | --- | --- | --- |
| 0 | 8 | `page_id` | このページの識別子(リトルエンディアン) |
| 8 | 1 | `page_type` | `0`(Meta)または`1`(Data) |
| 9 | 3 | (予約領域) | 常に0。将来のページ種類の拡張用 |
| 12 | 4 | `checksum` | ページ全体に対する`crc32`(リトルエンディアン) |
| 16 | `PAGE_PAYLOAD_SIZE` | `payload` | ページ本体 |

`PAGE_HEADER_SIZE`は16、`PAGE_PAYLOAD_SIZE`は`PAGE_SIZE - PAGE_HEADER_SIZE`(4080)です。
1バイトしか使わない`page_type`のあとに3バイトの予約領域を置いているのは、後続のフィールド(`checksum`)を4バイト境界に揃えるためと、将来`page_type`を拡張する余地を残すためです。

## Checksumで何を検出できるか

`encode`の最後で計算している`checksum`は、そのページのバイト列が書き込まれてから読み込まれるまでの間に、意図しない変化を受けていないかを確かめるための値です。
`crc32`という関数は、任意のバイト列を受け取り、32ビットの数値1つに要約します。

```rust
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in bytes {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}
```

これはCRC-32(IEEE 802.3、多項式`0xEDB88320`)という、業界で広く使われているアルゴリズムそのものです。
標準のCRC-32と同じ値を返すため、他のツールで計算した値とも突き合わせられます。
実装は1バイトごとに8回ビットシフトする素朴な形にとどめ、`crc32fast`のような専用クレートや、高速化のための事前計算テーブルは使いません。
この章で書き込むページ数(テストで扱う数ページから、数百ページ程度)では、素朴な実装でも速度上の問題にはなりません。
テーブル参照を導入すると、学ぶべき対象がCRC-32の計算そのものから、テーブルをどう事前生成するかという別の話題にずれてしまいます。
`serde`や`bincode`を避けた理由(依存を増やさず、バイト単位の対応を自分のコードで説明できる状態を保つ)は、ここでも同じ形で当てはまります。

`Page::decode`は、`encode`と逆の手順でバイト列から`Page`を復元しながら、この`checksum`を検証します。

```rust
pub fn decode(bytes: &[u8]) -> DbResult<Self> {
    if bytes.len() != PAGE_SIZE {
        return Err(DbError::CorruptPage(format!(
            "ページのバイト数が不正です: {PAGE_SIZE}バイトが必要ですが{}バイトでした",
            bytes.len()
        )));
    }

    let stored_checksum = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
    let mut zeroed = [0u8; PAGE_SIZE];
    zeroed.copy_from_slice(bytes);
    zeroed[12..16].fill(0);
    let actual_checksum = crc32(&zeroed);
    if stored_checksum != actual_checksum {
        return Err(DbError::CorruptPage(format!(
            "ページのchecksumが一致しません: {actual_checksum}が期待されましたが{stored_checksum}が記録されていました"
        )));
    }

    let page_id = PageId(u64::from_le_bytes(bytes[0..8].try_into().unwrap()));
    let page_type = PageType::from_u8(bytes[8])?;
    let payload = bytes[PAGE_HEADER_SIZE..].to_vec();

    Ok(Page {
        page_id,
        page_type,
        payload,
    })
}
```

読み込んだバイト列に記録されている`stored_checksum`を取り出したあと、同じバイト列の`checksum`フィールドだけを0で埋め直し、あらためて`crc32`を計算しています。
`encode`が`checksum`を計算する時点でも`buf[12..16]`はまだ0のままだったので(計算後に初めて書き込まれる)、これは`encode`のときと同じ入力に対して同じ計算をやり直していることになります。
この2つの値が一致すれば、`page_id`、`page_type`、`payload`のいずれについても、書き込まれたバイト列と読み込んだバイト列が一致していると判断できます。
`page_id`や`page_type`もchecksumの計算対象に含めているのは、`payload`だけでなくヘッダー部分の破損も同じ仕組みで検出するためです。

一致しなければ`DbError::CorruptPage`を返します。
この章で`DbError`に追加した唯一の新しいバリアントで、Magic Number不一致、Format Version不一致、checksum不一致、バイト数不一致、未知のPage Typeという、この章で起こりうる全ての壊れ方をまとめて表します。

```rust
/// File HeaderまたはPageのバイト列が壊れているエラー(Magic Number不一致、
/// Format Version不一致、checksum不一致、バイト数不一致、未知のPage Typeなど)。
#[error("破損したページです: {0}")]
CorruptPage(String),
```

`FileHeader::decode`も同じ考え方で、Magic Number、Format Version、checksumの3つを順に検証してから`FileHeader`を返します。

```rust
pub fn decode(bytes: &[u8]) -> DbResult<Self> {
    if bytes.len() != FILE_HEADER_SIZE {
        return Err(DbError::CorruptPage(format!(
            "File Headerのバイト数が不正です: {FILE_HEADER_SIZE}バイトが必要ですが{}バイトでした",
            bytes.len()
        )));
    }

    let magic = &bytes[0..4];
    if magic != MAGIC {
        return Err(DbError::CorruptPage(format!(
            "Magic Numberが一致しません: {:?}が期待されましたが{:?}でした",
            String::from_utf8_lossy(&MAGIC),
            String::from_utf8_lossy(magic)
        )));
    }

    let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if format_version != FORMAT_VERSION {
        return Err(DbError::CorruptPage(format!(
            "Format Versionが一致しません: {FORMAT_VERSION}が期待されましたが{format_version}でした"
        )));
    }

    let page_size = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let page_count = u64::from_le_bytes(bytes[12..20].try_into().unwrap());

    let stored_checksum = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    let actual_checksum = crc32(&bytes[0..20]);
    if stored_checksum != actual_checksum {
        return Err(DbError::CorruptPage(format!(
            "File Headerのchecksumが一致しません: {actual_checksum}が期待されましたが{stored_checksum}が記録されていました"
        )));
    }

    Ok(FileHeader {
        page_size,
        page_count,
    })
}
```

Magic NumberとFormat Versionの検証は、`checksum`の検証よりも意味が異なります。
この2つは、そもそも「読み込もうとしているファイルが、このコードの想定する形式かどうか」を判定するためのもので、正しい`minidb`のファイルであっても、Format Versionが古いバージョンのものなら一致しません。
一方`checksum`の検証は、「形式は合っている前提で、このバイト列自体が壊れていないか」を確かめるものです。
`decode`がこの順序(Magic Number → Format Version → checksum)で検証しているのは、形式そのものが違うファイルに対してまで、意味のない`checksum`計算を行わないようにするためです。

ここで、checksumが**何を検出できないか**にも触れておく必要があります。
`crc32`は32ビットの値なので、異なる入力から同じchecksumが偶然生まれる可能性(衝突)がゼロではありません。
また、checksumは「バイト列が壊れているかどうか」を検出するだけで、「なぜ壊れたか」も「どこが壊れたか」も教えてくれません。
ディスクの物理的な故障、電源断による書き込みの中断、あるいは`minidb`自身のバグでたまたま正しくない`payload`を書き込んでしまった場合、そのどれもが「checksumは一致するが、記録されている値の意味は正しくない」という形で通り抜けてしまうことがあります。
checksumが検出できるのは、あくまで「書き込んだ時点のバイト列」と「読み込んだ時点のバイト列」が一致しているかどうかという、狭い範囲の性質1つだけです。
ある時点で正しく書き込まれたページが、その後に壊れて読み込まれる、という事故に対しては有効ですが、書き込む前から論理的に誤っていた値を救うことはできません。

## テストで確認する

`page`モジュールには、`FileHeader`と`Page`それぞれについて、`encode`してから`decode`すると元の値に戻ることを確認するラウンドトリップのテストを用意しています。

```rust
#[test]
fn page_round_trip() {
    let mut page = Page::new(PageId(7), PageType::Data);
    page.payload_mut()[0..5].copy_from_slice(b"hello");
    let bytes = page.encode();
    let decoded = Page::decode(&bytes).unwrap();
    assert_eq!(decoded, page);
}
```

`crc32`自体にも、既知の入力に対する既知の出力を確認するテストを1つ加えています。

```rust
#[test]
fn crc32_matches_known_vector() {
    // "123456789"に対するCRC-32(IEEE 802.3)の既知の値。
    // <https://www.rfc-editor.org/rfc/rfc3720> Appendix B.4などで確認できる、
    // CRC-32の実装検証によく使われる定番の入力。
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}
```

`"123456789"`という9バイトの文字列は、CRC-32の実装が正しいかどうかを確かめるためによく使われる入力です。
このテストが通ることは、この`crc32`が標準的なCRC-32アルゴリズムと同じ結果を返すこと(自作の実装が独自の計算方法にすり替わっていないこと)を保証します。

## 壊して確認する

ラウンドトリップのテストは、`encode`と`decode`が互いに正しく対応していることは確認できますが、`decode`が本当に壊れたバイト列を拒否できているかまでは確認できません。
`encode`と`decode`のどちらもが同じ間違った前提を共有していれば、ラウンドトリップは黙って通ってしまいます。

そこで、正しく`encode`したバイト列を1バイトだけ意図的に書き換えてから`decode`に渡すテストを、Magic Number、Format Version、checksum、Page Typeのそれぞれについて用意しています。

```rust
#[test]
fn page_decode_rejects_corrupted_header() {
    let page = Page::new(PageId(1), PageType::Data);
    let mut bytes = page.encode();
    // page_idを壊す(checksumの対象に含まれるため検出できる)。
    bytes[0] ^= 0xFF;
    let err = Page::decode(&bytes).unwrap_err();
    assert!(matches!(err, DbError::CorruptPage(_)));
}
```

`page_id`を保持しているのはバイト列の先頭8バイトで、`payload`ではなくPage Headerの一部です。
このテストが確認しているのは、checksumの計算範囲がPage Header自身も含んでいる(`payload`だけを保護する設計ではない)ということです。
仮に`checksum`が`payload`にしか対応していなければ、`page_id`を書き換えてもこのテストは失敗を検出できず、赤くなるはずのテストが緑のまま通ってしまいます。
実際に`bytes[0] ^= 0xFF`のあと`Page::decode`を実行すると、`stored_checksum`と`actual_checksum`が一致せず、期待どおり`DbError::CorruptPage`が返ります。

`payload`側の破損も、同じ形のテストで確認できます。

```rust
#[test]
fn page_decode_rejects_corrupted_payload() {
    let page = Page::new(PageId(1), PageType::Data);
    let mut bytes = page.encode();
    // ヘッダーより後ろ(payload領域)を1バイトだけ壊す。
    let last = bytes.len() - 1;
    bytes[last] ^= 0xFF;
    let err = Page::decode(&bytes).unwrap_err();
    assert!(matches!(err, DbError::CorruptPage(_)));
}
```

ページの最後の1バイトは`payload`の末尾であり、Page Headerからは最も遠い位置にあります。
このバイトを壊してもchecksumの不一致として検出できることは、checksumの計算範囲が`payload`全体(4080バイト全て)をカバーしていることを確認しています。
`page_decode_rejects_corrupted_header`と`page_decode_rejects_corrupted_payload`を合わせて読むと、Page Headerと`payload`のどちらの破損も同じ1つの`checksum`検証で検出できることが分かります。

## 到達点

この章で作った`page`モジュールは、まだファイルを1バイトも読み書きしていません。
`FileHeader`と`Page`は、メモリ上の構造体とバイト列を相互変換できるだけの、独立した部品です。
このバイト列を実際のファイルへ書き出し、指定したページ番号のバイト列だけを読み出す仕事は、次の第13章のDisk Managerに引き継ぎます。

それでも、この章によって次の章以降の設計の骨格は固まりました。
ファイルは`PAGE_SIZE`バイトごとの区画に分かれていること、先頭の1区画がFile Headerであること、各ページの先頭16バイトが`page_id`、`page_type`、`checksum`を持つこと、そして壊れたバイト列は`decode`の時点で`DbError::CorruptPage`として検出できること。
これらはいずれも、この章のテストがバイト単位で検証済みの前提です。

## 演習問題

### 必須課題

1. `Page::encode`と`Page::decode`を読み、`checksum`フィールド(`bytes[12..16]`)自身が計算対象に含まれていない理由を説明してください。もし`checksum`フィールド自身も0埋めせずに計算対象へ含めてしまうとどうなるか、実際にコードを書き換えて`page_round_trip`を実行し、何が起きるか確認してください。
2. `FileHeader`には`page_count`というフィールドがありますが、この章のコードには`page_count`の値が正しいかどうかを検証する処理がありません(たとえば、実際のファイルサイズが`page_count × page_size`と一致するかどうかは確認していません)。この検証がこの章の`FileHeader::decode`に無い理由と、どの章でこの検証が可能になるかを、`src/page.rs`のドキュメントコメントを参考に説明してください。
3. `page_decode_rejects_unknown_page_type`のテストを読み、`Page::decode`が`page_type`のバイト(`bytes[8]`)を検証する前に、まず`checksum`の検証を通していることを確認してください。もし検証の順序を逆にして「`page_type`を先に検証し、checksumはあとで検証する」ように書き換えると、`page_decode_rejects_corrupted_header`や`page_decode_rejects_corrupted_payload`のようなテストにどんな影響がありうるか考えてください。

### 発展課題

1. この章の`crc32`は、`payload`が4080バイトあっても、その全バイトを毎回1バイトずつ処理します。標準的なCRC-32の高速化手法である「256エントリの参照テーブルを事前に計算しておき、1バイトごとの計算をテーブル引きに置き換える」実装を書き、この章の`crc32_matches_known_vector`と同じ既知の値を返すことを確認してください。
2. `PAGE_SIZE`を`8192`に変更すると、既存のテストのうちどれが壊れるか(あるいは全く壊れないか)を実際に確認してください。壊れるテストがあれば、それがなぜ`PAGE_SIZE`という定数に依存してしまっていたのかを説明してください。
3. `FileHeader`と`Page`はどちらも同じ`crc32`関数を使い、同じ「checksumフィールドだけを0埋めしてから計算する」という手順を踏んでいますが、この手順を共通の関数として括り出してはいません。共通化するとしたら、どのようなシグネチャの関数にすべきか設計し、実装してみてください。
