# 第33章 Write-Ahead Logging

`COMMIT`が`Ok`を返したら、その変更はもう安全だと考えたくなります。

```console
minidb> BEGIN;
BEGIN
minidb> INSERT INTO accounts VALUES (1, 100);
INSERT 1
minidb> COMMIT;
COMMIT
```

この直後にプロセスが落ちても、次に開いたときには残高100の行が読めるはずだ、という期待です。
実際に試してみます。

```rust
let mut db = Database::open(&path).unwrap();
db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
db.flush().unwrap();

db.execute("BEGIN").unwrap();
db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();
db.execute("COMMIT").unwrap();
// db.flush()を呼ばずにここでdropする。COMMIT直後にプロセスが死んだ状況を模す。
```

`db`をスコープの外へ出してdropすると、`COMMIT`直後にプロセスが死んだ状況を模せます。
そのあとで同じファイルを開き直し、先ほどの行を探してみます。

```rust
let mut db = Database::open(&path).unwrap();
let result = db.execute("SELECT balance FROM accounts WHERE id = 1").unwrap();
assert!(result.rows().is_empty());
```

行は見つかりません。
第30章の`execute_commit`は`self.tx = None`とするだけで、`backend`をディスクへ書き戻す操作を何一つ呼んでいませんでした。
第14章の`BufferPool`も第15章の`Storage`も、dirtyなページを書き戻す(`flush`)タイミングを呼び出し側の明示的な操作に委ねる設計を選んでおり、`COMMIT`はその呼び出し側には数えられていません。
`INSERT`が書いたページは、`BufferPool`のキャッシュにdirtyのまま残り続け、それを持っていたプロセスが終了すれば、ページの中身ごと消えます。
第13章で`DiskManager::sync`を実装したとき、この`sync`を「いつ呼ぶか」は後の章に持ち越すとだけ書きました。
持ち越した先が、この章です。

## 全ページを毎回同期する、という素朴な案

一番単純な直し方は、`COMMIT`のたびに`Database::flush`(`BufferPool::flush_all`と`DiskManager::sync`)を呼ぶことです。
これでも「壊して確認する」テストは通ります。
`Active`なトランザクションが書き換えたページが、`COMMIT`の時点で全部ディスクに同期されるからです。

ただし、この案は`COMMIT`のたびに**そのトランザクションが触れていないページ**まで同期してしまいます。
`BufferPool`はテーブル1つにつき専用のキャッシュを持つわけではなく、複数のテーブルのページが同じ`BufferPool`に混在します(第15章)。
`flush_all`はdirtyなページを見境なく全部書き戻すため、口座テーブルの1行を書き換えただけの`COMMIT`が、たまたま同じキャッシュに乗っていた無関係な注文テーブルの未確定ページまで同期してしまいます。
同期(`fsync`)はディスクI/Oの中でも特に遅い部類の操作であり、無関係なページを巻き込むほど、この案は`COMMIT`を重くします。

必要なのは、「このトランザクションが約束した変更」だけを指し示せる、ページよりも小さい単位です。
それが**ログレコード**であり、ログレコードを専用のファイルへ先に書き切ってから初めて`COMMIT`を確定させる方式が、この章のWrite-Ahead Logging(WAL)です。

## ログレコードの構成

WALは、データファイルとは別のファイル(`<db_path>.wal`)に、追記だけで育つログレコードの列を持ちます。
1件のログレコードは、次の情報を持ちます。

- **LSN**(**Log Sequence Number**)：そのレコードに割り当てる、単調増加の番号
- **Prev LSN**：同じトランザクションが直前に書いたレコードのLSN(最初のレコードは`None`)
- **Transaction ID**：どのトランザクションが書いたか
- **Record Type**：`Begin`、`Insert`、`Update`、`Delete`、`Commit`、`Abort`のいずれか
- **対象**：`TableId`と`RecordId`(第13章)の組
- **Before Image**、**After Image**：書き換え前、書き換え後のタプルのバイト列

```rust
pub struct LogRecord {
    pub lsn: Lsn,
    pub prev_lsn: Option<Lsn>,
    pub txn_id: TransactionId,
    pub record_type: LogRecordType,
    pub table_id: Option<TableId>,
    pub rid: Option<RecordId>,
    pub old_rid: Option<RecordId>,
    pub before_image: Option<Vec<u8>>,
    pub after_image: Option<Vec<u8>>,
}
```

`old_rid`だけは`対象`の説明に出てきませんでした。
`Storage::update`は、新しい値が元のページに収まらないとき、その行を別のページへ移動させます(第15章)。
`Update`レコード1件だけで「更新前はどこにあったか」と「更新後はどこにあるか」の両方を表すには、`RecordId`が2つ要ります。
`rid`を更新後の位置、`old_rid`を更新前の位置に使うことで、この移動を1件のレコードのまま表せます。
第30章の`UndoRecord::Update`が最初から`old_rid`と`new_rid`という2つのフィールドを持っていたのも、同じ理由でした。

### ページ単位ではなくタプル単位を選んだ理由

教科書のARIESの説明は、しばしばページ全体のバイト差分を記録する、ページ単位の物理ログを扱います。
このクレートは、`Storage`の書き込みがすでに`SlottedPage`の1スロットへの挿入、更新、削除として表現されている(第12章)ことに合わせ、ログレコードもタプル単位にしました。
`crate::executor`は新旧の`Tuple`をすでに手元に持っているので、これをそのままエンコードすればBefore/After Imageになります。
ページ単位にすると、この値をわざわざページ内オフセットへの差分へ変換し直す作業が必要になり、得られるものに対して手間が見合いません。

## LSNの採番とtorn writeへの備え

ログレコードは、`WalWriter`が手書きのリトルエンディアンでエンコードし、追記専用のファイルへ書きます。

```rust
pub struct WalWriter {
    file: File,
    buffer: Vec<u8>,
    records: Vec<LogRecord>,
    index: HashMap<Lsn, usize>,
    next_lsn: u64,
    durable_lsn: u64,
}
```

`append_*`系のメソッドは、LSNを1つ採番してレコードを組み立て、エンコードした結果を`buffer`(メモリ上)へ積むだけで、まだファイルには触れません。
`flush`が`buffer`をファイルへ書き渡し(OSのページキャッシュまで)、`sync`がそれに続けて`File::sync_all`を呼び、実ディスクへの同期を待ちます。
`DiskManager`(第13章)が採った「書き込みと同期を分ける」設計を、ログにもそのまま引き継いでいます。

1レコードのバイト列は、先頭に総バイト数を持つ形でエンコードします。

```text
[len: u32][lsn, prev_lsn, txn_id, record_type, table_id, rid, old_rid, before_image, after_image][checksum: u32]
```

`checksum`は`crc32`(第11章の`Page::encode`と同じアルゴリズム)です。
ファイルへの1回の書き込みの途中でプロセスやOSが落ちると、末尾に「長さは足りているが中身が壊れている」、あるいは「長さ自体が足りていない」バイト列が残ることがあります。
これが**torn write**です。

```rust
pub fn decode_stream(bytes: &[u8]) -> (Vec<LogRecord>, usize) {
    let mut records = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        match LogRecord::decode_one(&bytes[offset..]) {
            Some((record, consumed)) => {
                records.push(record);
                offset += consumed;
            }
            None => break,
        }
    }
    (records, offset)
}
```

`decode_stream`は1レコードずつ長さとchecksumを検証しながら読み進め、どちらかに失敗した時点で止まります。
返り値は「読めた完全なレコードの列」と「そこまでの有効なバイト数」の組であり、それより後ろに残っているバイト列がtorn writeです。
`WalWriter::open`は、ファイルを開くたびにこれを使って末尾のtorn writeを検出し、有効なバイト数のところまで`File::set_len`で切り詰めます。

```rust
let (records, valid_len) = decode_stream(&bytes);
if valid_len < bytes.len() {
    file.set_len(valid_len as u64)?;
    file.sync_all()?;
}
```

切り詰めた事実自体も同期しておかないと、その切り詰めがまた次のクラッシュで消えてしまいます。

## WALファースト不変条件

ログの形式が決まったので、次はそれをいつ使うかです。
この章が守る不変条件は、次の1文に尽きます。

```text
あるページの変更をディスクへ書き出す前に、
その変更を表すログレコードをディスクへ先に書き出す。
```

ログはページよりずっと小さく、常に末尾への追記だけで済みます。
この規律さえ守れば、ページの変更自体がまだディスクに届いていなくても、対応するログレコードさえ残っていれば、後からそのログを読んで変更を再現できます(この「読んで再現する」Redoは第34章の仕事です)。

この不変条件を強制するには、あるページの変更が「どのログレコードまで書けば再現できるか」を、ページ自身に覚えさせる必要があります。
それが**Page LSN**です。

```rust
struct FrameMeta {
    occupant: Option<PageId>,
    pin_count: u32,
    dirty: bool,
    referenced: bool,
    page_lsn: Lsn,
}
```

`BufferPool`(第14章)の各フレームに、`page_lsn`というフィールドを1つ追加しました。
`Storage::insert`、`update`、`delete`は、対応するログレコードを書いた直後に、そのページの`page_lsn`をそのレコードのLSNまで引き上げます。

```rust
let lsn = wal.append_insert(table_id, rid, bytes);
storage.stamp_page_lsn(rid.page_id, lsn);
```

`BufferPool`がdirtyなページを書き戻す経路は2つあります。
`flush_page`(呼び出し側の明示的な要求)と、`evict`(Clock置換によるキャッシュからの追い出し)です。
どちらも、実際に`DiskManager::write_page`を呼ぶ直前に、そのページの`page_lsn`まで`WalWriter::sync_up_to`を呼びます。

```rust
fn flush_frame(&self, frame_id: usize) -> DbResult<()> {
    let (page_lsn, wal) = {
        let inner = self.lock_inner();
        (inner.meta[frame_id].page_lsn, inner.wal.clone())
    };
    if let Some(wal) = wal {
        wal.lock().unwrap_or_else(|p| p.into_inner()).sync_up_to(page_lsn)?;
    }

    let frame = self.lock_frame(frame_id);
    if let Some(page) = frame.page.as_ref() {
        self.disk.write_page(page)?;
    }
    // ...
}
```

`sync_up_to`は、指定したLSNがすでに同期済みなら何もせず、まだなら`sync`を呼びます。

```rust
pub fn sync_up_to(&mut self, lsn: Lsn) -> DbResult<()> {
    if lsn.0 <= self.durable_lsn {
        return Ok(());
    }
    self.sync()?;
    Ok(())
}
```

これで、evictでもflushでも、どの経路を通ってページがディスクへ届こうとしても、そのページの変更を表すログが必ず先に同期されます。
`BufferPool`と`WalWriter`は別々の`Mutex`で守っており、`sync_up_to`を呼ぶ間はフレーム自体のロックを握っていないため、ログの同期に時間がかかっても他のフレームの読み書きを止めません。

`BufferPool::attach_wal`を呼ばなければ、この強制は一切働きません。
`Storage::create`、`Storage::open`は、テーブル本体用の`BufferPool`にだけこれを呼びます。
索引ごとの`BTree`が持つ`BufferPool`には結線していません。
索引ページの書き戻しにはこの章のWALファースト不変条件が及ばないという、この章が明示的に選んだ範囲の限定です(「この章の限界」で改めて触れます)。

## COMMITの耐久性

WALファースト不変条件は、ページより先にログが届くことを保証します。
それだけでは、`COMMIT`が返った時点で何が保証されているかがまだ決まりません。
`COMMIT`は、次の順序を守ります。

```rust
fn execute_commit(&mut self, _commit: CommitStatement) -> DbResult<QueryResult> {
    match &self.tx {
        None => Err(DbError::NoActiveTransaction),
        Some(tx) if tx.state == TransactionState::Aborted => Err(aborted_error(tx.victim_of_deadlock)),
        Some(_) => {
            let tx = self.tx.take().expect("直前のmatchでSomeを確認済み");
            wal_commit_if_disk(&self.backend, tx.id, tx.wal_last_lsn)?;
            self.lock_manager.release_all(tx.id);
            Ok(QueryResult::command("COMMIT"))
        }
    }
}
```

```rust
fn wal_commit_if_disk(backend: &Backend, tx_id: TransactionId, wal_last_lsn: Option<Lsn>) -> DbResult<()> {
    let Backend::Disk { storage } = backend else { return Ok(()) };
    let Some(last_lsn) = wal_last_lsn else { return Ok(()) };
    let mut w = storage.wal().lock().unwrap_or_else(|p| p.into_inner());
    w.append_commit(tx_id, Some(last_lsn));
    w.sync()?;
    Ok(())
}
```

`Commit`レコードを書き、`sync`が返ってくるのを待ってから、ようやく`Ok(QueryResult::command("COMMIT"))`を返します。
`sync`は`File::sync_all`まで呼ぶので、この関数が返った時点で、このトランザクションが書いたすべてのログレコード(`Begin`、`Insert`、`Update`、`Delete`、そして今書いた`Commit`自身)がディスク上に確定しています。
呼び出し元が`COMMIT`の成功を受け取った時点で、テーブル本体のページがまだディスクに届いていなくても、そのトランザクションが何をしたかを示す記録は失われません。

`wal_last_lsn`が`None`のとき(このトランザクションが1件も書き込んでいないとき)は、`Commit`レコードを書きません。
`SELECT`だけで終わったトランザクションには、そもそも同期して守るべき変更が無いためです。

### 明示的な`BEGIN`を伴わない1文も、それ自体が耐久性を持つ

`BEGIN`を書かずに実行した1文(Autocommit)にも、同じ規律を適用します。

```rust
fn run_disk_dml<F>(
    storage: &mut Storage,
    tx: &mut Option<TransactionContext>,
    next_txn_id: &mut u64,
    f: F,
) -> DbResult<usize>
where
    F: FnOnce(&mut Storage, &mut WalCursor) -> DbResult<usize>,
{
    let wal = storage.wal().clone();
    let autocommit = tx.is_none();
    let txn_id = tx.as_ref().map(|ctx| ctx.id).unwrap_or_else(|| {
        let id = TransactionId(*next_txn_id);
        *next_txn_id += 1;
        id
    });

    let mut local_prev_lsn: Option<Lsn> = None;
    let prev_lsn: &mut Option<Lsn> = match tx.as_mut() {
        Some(ctx) => &mut ctx.wal_last_lsn,
        None => &mut local_prev_lsn,
    };

    let result = {
        let mut cursor = WalCursor::new(&wal, txn_id, prev_lsn);
        f(storage, &mut cursor)
    };

    if autocommit && let Some(last_lsn) = *prev_lsn {
        let mut w = wal.lock().unwrap_or_else(|p| p.into_inner());
        match &result {
            Ok(_) => {
                w.append_commit(txn_id, Some(last_lsn));
                w.sync()?;
            }
            Err(_) => {
                w.append_abort(txn_id, Some(last_lsn));
            }
        }
    }
    result
}
```

`INSERT`、`UPDATE`、`DELETE`のDisk側の実装は、すべてこの関数を経由します。
`tx`が`Some`(明示的な`BEGIN`の中)であれば、この関数はまだ何も確定させません。
`COMMIT`、`ROLLBACK`が届くまで、ログレコードはただ積み上がるだけです。
`tx`が`None`(Autocommit)であれば、この1文だけのための使い捨てのトランザクションIDを採番し、成功すれば`Commit`を書いて`sync`し、失敗すれば`Abort`を書いて終わります。
`Begin`レコード自体は、実際に1件でも書き込みが起きた時点で`WalCursor`が遅延して書きます。

```rust
fn ensure_begin(&mut self, wal: &mut WalWriter) {
    if self.prev_lsn.is_none() {
        let lsn = wal.append_begin(self.txn_id);
        *self.prev_lsn = Some(lsn);
    }
}
```

`WHERE`に一致する行が1件も無かった`UPDATE`、`DELETE`は、`Begin`すら書きません。
`run_disk_dml`は、`prev_lsn`が`None`のままかどうかを見るだけで、「このトランザクションは何か書いたか」を過不足なく判定できます。

## ROLLBACKをWALベースへ書き直す

第30章のロールバックは、`INSERT`、`UPDATE`、`DELETE`のたびに逆操作(`UndoRecord`)をプロセスのメモリへ積み、`ROLLBACK`が届いたらそれを逆順に適用していました。
このUndoはディスクに何も書かないため、`Active`なトランザクションの途中でクラッシュすれば、それまでの変更が`backend`にどこまで反映されていたかを知る手段が無いまま、Undoの記録ごと失われます。

WALはすでに、`Insert`、`Update`、`Delete`のBefore/After Imageを1件ずつ持っています。
`ROLLBACK`は、プロセスのメモリに別々の`Vec`を積む代わりに、このログレコードをたどるだけで逆操作を再現できます。

```rust
pub(crate) fn apply_wal_undo_disk(storage: &mut Storage, last_lsn: Option<Lsn>) -> DbResult<()> {
    let records = {
        let wal = storage.wal().lock().unwrap_or_else(|p| p.into_inner());
        let mut records = Vec::new();
        let mut current = last_lsn;
        while let Some(lsn) = current {
            let record = wal.record(lsn).expect("wal_last_lsn・prev_lsnは常にWalWriterへ記録済みのLsnを指す").clone();
            if record.record_type == LogRecordType::Begin {
                break;
            }
            current = record.prev_lsn;
            records.push(record);
        }
        records
    };
    // ... Insert・Update・Deleteそれぞれの逆操作を、第30章のapply_undo_diskと
    // 同じ`remap`(RecordIdの付け替え)を使って適用する。
    Ok(())
}
```

`last_lsn`は`TransactionContext::wal_last_lsn`、このトランザクションが直近に書いたレコードのLSNです。
そこから`prev_lsn`を`Begin`レコードに行き着くまでたどると、たどった順序はこのトランザクションが実際に書き込んだ順とちょうど逆順、つまりLIFOになります。
`RecordId`の付け替え(`remap`)が要る理由も、索引の更新順序も、失敗時に中途半端な状態が残りうる割り切りも、すべて第30章の`apply_undo_disk`とまったく同じです。
違うのは、逆操作の元になる値を、プロセスのメモリに積んだ`Vec`からではなく、WALが保持しているログレコードから読む点だけです。

### `UndoRecord`はMemoryバックエンド専用になった

Diskバックエンドが`apply_wal_undo_disk`へ切り替わったことで、第30章の`apply_undo_disk`はもう誰からも呼ばれません。
`UndoRecord`という型自体は、`Vec<Tuple>`の並びでしかなくディスクに何も書かないMemoryバックエンド向けの実装として残しました。

```rust
#[derive(Debug, Clone)]
pub enum UndoRecord {
    Insert { table_id: TableId, tuple: Tuple },
    Delete { table_id: TableId, tuple: Tuple },
    Update { table_id: TableId, old: Tuple, new: Tuple },
}
```

`RecordId`を指す`rid`、`old_rid`、`new_rid`はもう要りません。
Diskバックエンドの記録でだけ`Some`になっていたフィールドだったので、Diskバックエンドがこの型を使わなくなった今、常に無意味な`None`を運ぶだけの荷物でした。
Memoryバックエンドはそもそも永続化しないデータベースであり、WALを持ち込む動機(クラッシュをまたいだ復元)自体がありません。
この章は、Memoryバックエンドの`UndoRecord`はそのまま維持するという線引きを選びました。

## 開発用ダンプでWALの中身を見る

この章はまだCrash Recoveryを実装しません。
ログを読んで状態を復元するRedo、Undoは第34章の仕事で、この章は「WALファーストを守ってログを先に書く」ところまでです。
それでも、書いたログが実際にディスクへ残っていることは目視で確認したいので、開発用のダンプを用意しました。

```rust
pub fn dump(&self) -> Vec<String> {
    self.records.iter().map(format_record).collect()
}
```

```console
minidb> BEGIN;
minidb> INSERT INTO accounts VALUES (1, 100);
minidb> COMMIT;
```

このあとで`db.wal_dump()`を呼ぶと、次のような行が返ります。

```text
lsn=1 prev=- txn=1 type=Begin - before=0B after=0B
lsn=2 prev=1 txn=1 type=Insert table=0 rid=(2,0) before=0B after=17B
lsn=3 prev=2 txn=1 type=Commit - before=0B after=0B
```

`Insert`レコードが17バイトのAfter Imageを持っていることが、この行から直接読み取れます。
第34章のRedoが読むことになる材料は、すでにこの形でディスク上に揃っています。

## クラッシュを再現してWALの生存を確かめる

章の冒頭の「壊して確認する」を、もう一度たどり直します。
今度は、`COMMIT`直後にプロセスが死んだあとの状態を、テーブル本体ではなくWALファイル側から覗きます。

```rust
let path = temp_db_path("wal-crash-keeps-the-log");
{
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE accounts (id BIGINT PRIMARY KEY, balance BIGINT NOT NULL)").unwrap();
    db.flush().unwrap();

    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO accounts VALUES (1, 100)").unwrap();
    db.execute("COMMIT").unwrap();
    // ここでも db.flush() は呼ばない。
}

let wal = minidb::WalWriter::open(wal_path(&path)).unwrap();
let dump = wal.dump();
assert!(dump.iter().any(|line| line.contains("type=Insert")));
assert!(dump.iter().any(|line| line.contains("type=Commit")));
```

テーブル本体を`SELECT`で読めば、章の冒頭と同じく行は見つかりません。
`Database::flush`を一度も呼んでいないので、`accounts`テーブルのページはまだ`BufferPool`のキャッシュに留まったままプロセスとともに消えています。
それでも、`WalWriter::open`でWALファイルを開き直すと、`Insert`と`Commit`のレコードは残っています。
コミットが実際に起きたという事実と、その行の中身(After Image)は生き延びました。
第34章がこの章に残す仕事は、この生き延びた記録を読んでテーブル本体を作り直すことです。

## テストで確認する

`src/wal.rs`には、ログレコードのencode/decode往復、torn tailの検出と切り詰め、`sync_up_to`が同期済みのLSNへ再同期しないことを確認する単体テストを追加しました。
`src/buffer_pool.rs`には、`flush_page`とeviction(Clock置換)のどちらの経路でも、ページを書き戻す前にそのページのPage LSNまでWALが同期されていることを確認するテストを追加しました。
`tests/wal_durability.rs`には、この章の一連の主張(COMMITはテーブル本体を同期しない、WALは同期する、COMMITはWALの同期を待ってから返る、Autocommitも同じ規律に従う、ROLLBACKはWALのBefore Imageで復元する)をそれぞれ確認する統合テストを追加しました。

```console
$ cargo test --lib
test result: ok. 786 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out
$ cargo test --test wal_durability
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

第30章から続く`tests/interleave.rs`、`tests/interleave_disk.rs`、`tests/isolation_levels.rs`、`src/database.rs`のトランザクション関連のテスト群も、ROLLBACKの実装がWALベースへ差し替わった状態のまま全て通ります。

## この章の限界

索引ごとの`BTree`が持つ`BufferPool`には、WALを結線していません。
`CREATE INDEX`、Index Maintenanceが書き換える索引ページは、この章のWALファースト不変条件の対象外です。
テーブル本体(Heap)の変更は必ずログに残りますが、索引ページの物理的な書き戻し順序はこれまでどおり`BufferPool`のClock置換任せであり、クラッシュ後に索引だけが古い状態のまま残る余地が残っています。
第34章のRedoが再現できるのはテーブル本体の変更だけなので、索引側をどう復元するか(索引の変更もログに残すか、索引だけ毎回作り直すか)は、この章では決めずに残します。

この章のWALは、書いたログレコードをプロセスのメモリ上(`WalWriter::records`)にも保持し続けます。
実際のRDBMSはこれを行わず、必要になったレコードをその都度ディスクから読み直します。
`ROLLBACK`が`prev_lsn`の連鎖を即座にたどれる必要があるという、この章の実装上の都合であり、プロセスの寿命を超えてこの連鎖をたどる必要が生じるのが、まさに第34章のCrash Recoveryです。
そこでは実際にファイルを読み直す設計に切り替わります。

`Abort`レコードは、`ROLLBACK`の中で書きはしますが、`sync`までは待ちません。
失敗した(取り消された)トランザクションの記録を急いで永続化する理由が無いためですが、`Abort`を書く直前にクラッシュすれば、そのレコード自体がまだディスクに届いていない可能性があります。
第34章のRecoveryが、`Commit`の無いトランザクションをどう扱うか(`Abort`があってもなくても未コミットとして一律にUndoする)を決めれば、この非対称は問題になりません。

## 演習問題

### 必須課題

1. `WalWriter::sync_up_to`は、指定したLSNがすでに`durable_lsn`以下なら何もしません。この早期リターンを取り除くとどうなるか(`sync`を無条件に呼ぶとどう変わるか)を、`flush_page_syncs_the_wal_up_to_the_pages_page_lsn_before_writing_it_back`のようなテストを書いて確認してください。正しさは保たれるか、何が変わるかを考えてください。
2. `run_disk_dml`は、Autocommitの1文が失敗すると`Abort`レコードを書きますが`sync`は呼びません。この`sync`を呼ぶように変更した場合、`COMMIT`のパフォーマンス上の得失以外にどんな意味の違いが生じるか(失敗した文の記録を急いで永続化する必要が本当に無いか)を考えてください。
3. `LogRecord::decode_one`から、checksumの検証(`crc32(body) != stored_checksum`のチェック)を取り除くとどうなるか、実際に試してください。`decode_stream_rejects_a_flipped_byte_in_the_last_record`がどう失敗するかを確認し、checksumが無いとtorn writeの検出がどう不完全になるかを説明してください。

### 発展課題

1. この章はテーブル本体の`BufferPool`にだけWALを結線し、索引の`BufferPool`には結線していません(「この章の限界」を参照)。索引ページの書き戻しにも同じWALファースト不変条件を及ぼすように、`BTree`が持つ`BufferPool`にも`attach_wal`を呼ぶ変更を設計、実装してください。索引ページの変更を表すログレコードをどう表現するか(`Record Type`を追加するか、既存の`Insert`、`Delete`を流用するか)から考える必要があります。
2. `WalWriter`は、これまでに`append`した全レコードをプロセスのメモリ上に保持し続けます(「この章の限界」を参照)。これを、直近のトランザクションぶんだけを保持し、それより古いレコードはファイルから読み直す実装に変更してみてください。`apply_wal_undo_disk`がファイルI/Oを行うようになった場合、そのエラーをどう`DbResult`へ伝播させるかも設計してください。
3. `wal_dump`が返す文字列は、`format_record`が1行ずつ整形しただけの簡易な表現です。これを、LSNの昇順ではなくトランザクションごとにグループ化して表示する(`Begin`から`Commit`、`Abort`までの1本のチェーンとしてまとめて表示する)ように書き換えてみてください。`prev_lsn`の連鎖をどうたどれば、この並べ替えが実現できるか考えてください。
