# 第34章 Crash RecoveryとCheckpoint

前章の最後で、WALには`Insert`と`Commit`のレコードが確かに残っていました。

```console
minidb> BEGIN;
minidb> INSERT INTO accounts VALUES (1, 100);
minidb> COMMIT;
```

`db.flush()`を一度も呼ばずにここでプロセスが死んだことにして、WALファイルだけを直接開き直すと、`Insert`と`Commit`のレコードが確かに残っています。
これは`tests/wal_durability.rs`にある、第33章のテストの一部です。

```rust
let wal = minidb::WalWriter::open(wal_path(&path)).unwrap();
let dump = wal.dump();
assert!(dump.iter().any(|line| line.contains("type=Insert")));
assert!(dump.iter().any(|line| line.contains("type=Commit")));
```

コミットが実際に起きたという事実と、その行の中身(After Image)は生き延びています。
第33章の時点では、ここで`Database::open`から`SELECT`し直しても、テーブル本体には行が1つも見当たりませんでした。
それなのに、生き延びているはずのその内容が、テーブル本体にはまだ戻ってこない。

理由は単純です。
`Storage::open`は、WALファイルを読み込んで`WalWriter`を作り直すだけで、そこに書いてある内容を読み返してテーブル本体へ反映し直す処理を一度も呼んでいませんでした。
記録は残っているのに、読む者がいない。
この章は、その読む者を実装します。

## ARIESを教材規模へ簡略化する

クラッシュ後にWALを読んでデータベースを復元する手順には、標準的な名前があります。
IBMの研究者たちが1990年代に定式化した**ARIES**(Algorithm for Recovery and Isolation Exploiting Semantics)です。
ARIESは3つの段階から成ります。

**Analysis**は、WALを先頭から末尾まで走査し、クラッシュ時点で何が起きていたかを再構成します。
具体的には、どのトランザクションがまだ確定していなかったか(**Transaction Table**)と、どのページがまだディスクに届いていない変更を持っていたか(**Dirty Page Table**)を突き止めます。

**Redo**は、WALに記録された変更を、実際に確定していたかどうかを問わず、ログの順番どおりにもう一度適用します。
これを**Repeating History**と呼びます。
一見奇妙に思えるかもしれません。
まだコミットしていない変更まで律儀に再現するのは無駄ではないか、という疑問はもっともです。
けれどもこの段階では、どのトランザクションが最終的に生き残るかをまだ気にしません。
クラッシュ直前の物理的な状態をそっくりそのまま再現することだけに専念し、生き残らないトランザクションの後始末は次の段階に譲ります。
こう役割を割り切ることで、Redoは「このページのこの変更が届いているかどうか」という1つの問いだけに集中できます。

**Undo**は、Analysisで「確定しなかった」と分かったトランザクション(**loser**)の変更を、Redoで再現し終えた状態から取り消します。

この3段階の枠組みは、`minidb`の規模でも通用します。
ただし、教科書のARIESが前提にする道具立てのいくつかは、この教材の分量に見合いません。
以下の表に、単純化した箇所と、その埋め合わせとして使った代替策をまとめます。

| ARIESが使う道具 | この章の代替 | 理由 |
| --- | --- | --- |
| Dirty Page Table(recLSNの最小値でRedoの開始点を絞る) | 各レコードの対象ページのPage LSNを都度読んで比較する | テーブルのページ数が教材規模(数百枚程度)にとどまり、総当たりでも十分安い |
| CLR(Compensation Log Record) | Recovery全体を「ディスクへの反映は最後に1回だけ」というアトミックな操作にする | Undo中に再クラッシュしても、ディスク上は`Storage::open`の直前と変わっていないため、次のRecoveryが最初からやり直せる |
| loserどうしのUndoをLSN降順で1本にインターリーブ | loserを1本ずつ、最後まで直列にUndoする | シングルスレッドかつStrict 2PL(第31章)のもとでは、複数のloserが同じ行を同時に保持できない |
| ページ単位の物理Redo(索引ページも含む) | 索引はHeapから丸ごと作り直す | 索引ページはWALと結線していない(第33章の限界)ため、個別に復元するより作り直すほうが単純で確実 |

これらの単純化は、どれも「この教材の前提のもとでは正しさを損なわない」という根拠つきの選択です。
以下、実際にどう実装したかを順に見ていきます。

## Page LSNをページ自身に刻む

Redoが「この変更はもう届いているか」を判定するには、ページの側にも何らかの目印が要ります。
第33章の`Page LSN`は、この目印そのものでした。
ただし、その持ち場は`BufferPool`の`FrameMeta`という、プロセスのメモリ上にしかない場所でした。
`src/buffer_pool.rs`の`FrameMeta`は、次のように`page_lsn`を持っています。

```rust
struct FrameMeta {
    occupant: Option<PageId>,
    pin_count: u32,
    dirty: bool,
    referenced: bool,
    page_lsn: Lsn,
}
```

プロセスが再起動すれば、この値は失われます。
クラッシュ後に読み直したページが「どこまでの変更をすでに含んでいるか」を、プロセスの寿命をまたいで覚えておく手段が無ければ、Redoは判定のしようがありません。

この章は、`Page LSN`をページのバイト列そのものに埋め込みます。
`src/page.rs`の`Page`に、次のように`page_lsn`フィールドを追加します。

```rust
pub struct Page {
    pub page_id: PageId,
    pub page_type: PageType,
    pub page_lsn: Lsn,
    payload: Vec<u8>,
}
```

Page Headerに8バイト増えた分、`PAGE_HEADER_SIZE`は16から24へ、`PAGE_PAYLOAD_SIZE`はその分だけ縮み、`FORMAT_VERSION`も1つ上げてあります。
`BufferPool`は、ページを新しく読み込むとき、ディスクに永続化されていた`page_lsn`をそのままフレームの初期値として引き継ぎます。
`src/buffer_pool.rs`には、次のように書き加えます。

```rust
        let page_lsn = page.page_lsn;
        self.lock_frame(frame_id).page = Some(page);
        inner.meta[frame_id] = FrameMeta {
            occupant: Some(id),
            pin_count: 0,
            dirty: false,
            referenced: false,
            page_lsn,
        };
```

逆に、同じ`src/buffer_pool.rs`で、dirtyなページを書き戻す直前には、フレームが覚えている最新の値をページ自身へ書き写してからディスクへ渡します。

```rust
        let mut frame = self.lock_frame(frame_id);
        if let Some(page) = frame.page.as_mut() {
            page.page_lsn = page_lsn;
            self.disk.write_page(page)?;
        }
```

これで、あるページがディスクへ書き戻されるたびに、「この変更まではもう反映済み」という事実がページ自身に刻まれます。
クラッシュしてプロセスが再起動しても、ページを読み直しさえすれば、この事実は失われません。
Redoが冪等に振る舞える根拠は、突き詰めればこの1つの永続化に尽きます。

## Analysis: Transaction Tableを組み立てる

`Storage::open`は、カタログと索引を読み込んだ直後、WALファイルの内容(`WalWriter::open`がすでに全レコードをメモリへ読み込んでいます)を使ってAnalysisを行います。
やることは単純です。
WALの記録を先頭から順に見ていき、トランザクションごとに「最後に書いたレコードのLSN」と「`Commit`か`Abort`をすでに見たかどうか」を追跡するだけです。

この章はAnalysis、Redo、Undoをまとめて、新規モジュール`src/recovery.rs`として実装します。
`src/recovery.rs`の`TxState`は、次のようにトランザクションごとの状態を持ちます。

```rust
struct TxState {
    last_lsn: Option<Lsn>,
    resolved: bool,
}
```

その中身は、次のように`TxState`を組み立てるところから始まります。

```rust
    for record in scanned {
        match record.record_type {
            LogRecordType::Commit | LogRecordType::Abort => {
                table
                    .entry(record.txn_id)
                    .and_modify(|s| {
                        s.last_lsn = Some(record.lsn);
                        s.resolved = true;
                    })
                    .or_insert(TxState { last_lsn: Some(record.lsn), resolved: true });
            }
            LogRecordType::Begin | LogRecordType::Insert | LogRecordType::Update | LogRecordType::Delete => {
                table
                    .entry(record.txn_id)
                    .and_modify(|s| s.last_lsn = Some(record.lsn))
                    .or_insert(TxState { last_lsn: Some(record.lsn), resolved: false });
            }
            LogRecordType::Checkpoint => {}
        }
    }
```

あわせて`src/lib.rs`に次の宣言を加え、このモジュールを公開します。

```rust
pub mod recovery;
```

走査を終えた時点で、`resolved`が`false`のまま残っているトランザクションが**loser**です。
`Commit`も`Abort`も記録されていない、つまりクラッシュの瞬間にActiveだったトランザクションだと分かります。

`TxState`の`last_lsn`は`Option<Lsn>`です。
`None`は「このトランザクションはまだ1件もWALレコードを書いていない」ことを表します。
`Checkpoint`の瞬間にActiveだったトランザクションをTransaction Tableの初期値として引き継ぐとき(次節「Checkpoint」を参照)、そのトランザクションがまだ何も書いていなければ`last_lsn`は`None`のまま引き継がれ、この走査ループが`Insert`、`Update`、`Delete`、`Commit`、`Abort`のどれかを実際に見つけるまで`None`であり続けます。
`Lsn`はWAL上に実在するレコードの番号であり、「まだ何も書いていない」ことを表すための架空の`Lsn`(たとえば`Lsn(0)`)を割り当てて代用してはいけません。
架空の`Lsn`をUndoの起点として渡すと、次節のUndoが`WalWriter::record`でその`Lsn`を引こうとして見つからず、`panic`します。

Dirty Page Tableに相当するものは、あえて作りません。
本来のARIESがDirty Page Tableを持つ理由は、「Redoはどのページのどこから始めればよいか」を、ログ全体を舐めずに絞り込むためです。
このクレートが扱うページ数は教材規模にとどまるため、Redoの各レコードについてその都度対象ページのPage LSNを読みに行く総当たりの判定で、実用上困る遅さにはなりません。
省いた道具の代わりに、前節で永続化した`Page LSN`をそのまま使う、という選択です。

## Redo: 位置ぴったりに書き戻す

Redoは、Analysisが決めた走査範囲のレコードを、LSNの昇順のまま1件ずつ再適用します。
`Insert`、`Update`、`Delete`のどれであっても、対象ページの現在のPage LSNがそのレコードのLSN以上であれば、もう反映済みなので何もしません。
`src/storage.rs`に、次の`redo_insert`を定義します。

```rust
    pub(crate) fn redo_insert(&mut self, table_id: TableId, rid: RecordId, bytes: &[u8], lsn: Lsn) -> DbResult<()> {
        let lsn_before = self.pool.page_lsn(rid.page_id)?;
        if lsn_before >= lsn {
            return Ok(());
        }
        {
            let mut guard = self.pool.write_page(rid.page_id)?;
            let mut page =
                if lsn_before.0 == 0 { SlottedPage::init(guard.data_mut()) } else { SlottedPage::open(guard.data_mut())? };
            let slot = page.insert(bytes);
            debug_assert_eq!(
                slot,
                Some(rid.slot_id),
                "Redoは元のRecordIdと同じスロットを再現できる前提(本文を参照)"
            );
            let free = page.free_space();
            drop(guard);
            self.fsm.update(rid.page_id, free);
        }
        self.pool.bump_page_lsn(rid.page_id, lsn);
        self.attach_page_if_missing(table_id, rid.page_id)?;
        Ok(())
    }
```

`crate::transaction::apply_wal_undo_disk`(第33章の`ROLLBACK`が使うUndo)は、`storage.insert`という**空いている場所を探す**通常の経路を使い、行が元と違う`RecordId`に移っても構いません。
移った先は`remap`という付け替え表で辻褄を合わせます。
Redoはそうはいきません。
このレコードより後ろに、同じ行を同じ`RecordId`で参照する別のレコード(同じトランザクションの`UPDATE`や`DELETE`)が続くかもしれないからです。
Redoは、WALが記録した`RecordId`そのものへ、寸分違わず書き戻す必要があります。

この位置ぴったりの書き戻しが事故なく成り立つのは、次の理由によります。
Redoは1本のWALを昇順にたどり、あるページに対して行う操作も必ずその順番のまま届きます。
あるページについて、すでに反映済みだとしてスキップするレコードは、常に「LSNが小さい側から連続した範囲」になります。
つまり、あるレコードをスキップした後にそのページへ戻ってきたときも、`SlottedPage`の状態は「スキップした操作をちょうどそこまで実行し終えた」ものとちょうど一致しています。
だからこそ、`SlottedPage::insert`が(空きスロットの再利用を優先し、無ければ末尾へ追加する)決まった規則でスロット番号を割り当てても、Redoが実際に書き込む番号は元の実行時とそろいます。

`lsn_before`が`Lsn(0)`のときだけ`SlottedPage::init`を使うのは、そのページが一度もこのプロセスの外で初期化されたことが無い(`allocate_page`で確保されたばかりの、全バイト0のページ)場合に備えるためです。
`Page LSN`がまだ0ということは、このページへの書き込みが1件も永続化されていないことの証拠だからです。

`Update`は、`old_rid`と`rid`が一致するかどうかで処理が分かれます。
一致すれば同じページ内で完結する更新、食い違えば「新しい位置へ挿入し、古い位置をtombstone化する」という2つの物理操作に分解されます。
この振り分けは`src/recovery.rs`に書きます。

```rust
            if old_rid == new_rid {
                let before = storage.page_lsn(new_rid.page_id)?;
                storage.redo_update_in_place(new_rid, after, record.lsn)?;
                Ok(before < record.lsn)
            } else {
                let before_new = storage.page_lsn(new_rid.page_id)?;
                let before_old = storage.page_lsn(old_rid.page_id)?;
                storage.redo_insert(table_id, new_rid, after, record.lsn)?;
                storage.redo_delete(old_rid, record.lsn)?;
                Ok(before_new < record.lsn || before_old < record.lsn)
            }
```

この2つの操作は、それぞれ別のページに属している可能性があるため、Page LSNの判定も独立に行います。
`crate::executor::storage_update`が、行の移動を伴う`UPDATE`のときに新旧両方のページへ`stamp_page_lsn`を呼んでいたのと、まったく同じ構図です。

## Undo: loserを取り消す

Redoが終わった時点で、テーブルはクラッシュ直前の物理的な状態(コミット済みかどうかを問わない、生の状態)まで復元されています。
残るのは、Analysisがloserと判定したトランザクションの変更を取り消すことです。

ここで使うのは、新しいコードではありません。
`src/recovery.rs`の`recover`は、第33章の`ROLLBACK`がすでに実装していた`crate::transaction::apply_wal_undo_disk`を、loserごとにそのまま呼び出します。

```rust
    let mut transactions_undone = 0usize;
    for (txn_id, state) in &table {
        if state.resolved {
            continue;
        }
        crate::transaction::apply_wal_undo_disk(storage, state.last_lsn)?;
        storage.wal().lock().unwrap_or_else(|p| p.into_inner()).append_abort(*txn_id, state.last_lsn);
        transactions_undone += 1;
        crate::failpoint::hit("recovery_undo_step")?;
    }
```

`ROLLBACK`が「今まさにActiveなトランザクションを、実行中のプロセスの中で」取り消すのと、`recover`が「クラッシュで凍結されたトランザクションを、開き直したプロセスの中で」取り消すのは、コードの視点からはまったく同じ操作です。
`apply_wal_undo_disk`は`last_lsn`から`prev_lsn`を`Begin`に行き着くまでたどり、`RecordId`の付け替え(`remap`)を使いながら`Insert`、`Update`、`Delete`それぞれの逆操作を適用します。
この`remap`が要る理由も、索引の更新順序も、第30章と第33章から変わっていません。
`last_lsn`が`None`のloser(1件も書き込んでいないまま`Active`で終わったトランザクション)は、`apply_wal_undo_disk`が最初の1歩を踏み出す前にループを抜けて即座に`Ok(())`を返すため、そのまま何も取り消さずに`Abort`レコードだけを書きます。

Undoを終えたトランザクションには`Abort`レコードを書きます。
`ROLLBACK`と同じ体裁ですが、1つだけ違いがあります。
`ROLLBACK`(`wal_rollback_if_disk`)はこの直後に`flush`を呼びますが、`recover`は呼びません。
理由は次の節で説明します。

## Undo中のクラッシュにも耐える

演習として素朴に考えると、次のような疑問が浮かびます。
Undoの途中で(つまりRecoveryそのものの実行中に)またプロセスが死んだら、どうなるのでしょうか。

本物のARIESは、この問いにCLR(Compensation Log Record)で答えます。
Undoの1操作ごとに専用のログレコードを書き、そのレコードが「どこまでUndo済みか」を次のRecoveryへ伝える仕組みです。
この教材はCLRを実装しません。
代わりに、`recover`全体を1つのアトミックな操作にすることで、同じ問題を解決します。

`recover`は、Analysis、Redo、Undoのすべてが終わるまで、`Storage::flush`と`Storage::sync`のどちらも呼びません。
Redoが書き込むページも、Undoが書き込むページも、Undoが積む`Abort`レコードも、この時点ではすべて`BufferPool`や`WalWriter`のメモリ上のバッファに留まっています。
`src/recovery.rs`の`recover`は、最後に次のようにまとめて反映します。

```rust
    storage.flush()?;
    storage.sync()?;
    for (temp_path, index_path) in &pending_index_renames {
        std::fs::rename(temp_path, index_path)?;
    }
    storage.wal().lock().unwrap_or_else(|p| p.into_inner()).sync()?;
```

この数行に到達して初めて、ここまでの全変更がディスクへ実際に反映されます。
逆に言えば、この手前のどこかで`recover`が失敗すれば(この章のテストでは`crate::failpoint`で意図的に発生させます)、`storage`ごと丸ごと破棄され、途中まで進んでいた変更はメモリ上から跡形もなく消えます。
ディスク上のバイト列は、`Storage::open`を呼ぶ直前と一切変わっていません。

### 索引ファイルの入れ替えは一時ファイル経由

この「ディスク上のバイト列が一切変わっていない」という主張には、実は索引ファイルという見落としやすい例外があります。
`rebuild_all_indexes_after_recovery`(Redoより前、本文「Analysis: Transaction Tableを組み立てる」の直後に呼びます)は、既存の索引ファイルを削除して同じパスへ作り直すのではなく、同じディレクトリの**一時ファイル**へ新しい索引を書きます。
既存の索引ファイル自体には、この時点では一切触れません。
一時ファイルを指す`BTree`をその場で`self.indexes`へ組み込むため、続くUndo(`apply_wal_undo_disk`が呼ぶ`index_insert_row`、`index_delete_row`)は、この一時ファイル上の`BTree`を正しく更新できます。

既存の索引ファイルを実際に置き換えるのは、上のコード片が示す`std::fs::rename`です。
同じディレクトリ内の`rename`はファイルシステムレベルで単一の操作であり、途中の中途半端な状態を外部から観測できません。
これで、索引ファイルもデータページ、カタログ、WALと同じく、「Redo、Undo、検証がすべて終わるまでディスク上のバイト列が変わらない」という不変条件に加わります。

次に`Storage::open`を呼び直すと、`recover`はまったく同じ入力(変化していないWAL、変化していないページ)から、Analysis、Redo、Undoを最初からやり直します。
Redoが冪等であることはすでに確認したとおりで、Undoも`apply_wal_undo_disk`をもう一度呼ぶだけです。
1回目の実行がどこまで進んでいたかを気にする必要が無いのは、1回目の実行の痕跡がディスク上のどこにも残っていないからです。
「途中経過を細かく記録して引き継ぐ」代わりに「途中経過を一切残さず、失敗したら最初からやり直す」という、この章が選んだ単純化です。

## CHECKPOINT: Analysisの開始点を前に進める

Analysisは、WALの先頭からすべてのレコードを見て回ります。
稼働時間が延びるほどWALは長くなり、Analysisが見て回る範囲も広がっていきます。
`CHECKPOINT`は、この範囲を短く保つための手段です。
`src/storage.rs`に、次の`checkpoint`を定義します。

```rust
    pub fn checkpoint(&mut self, active: &[(TransactionId, Option<Lsn>)]) -> DbResult<Lsn> {
        self.flush()?;
        self.sync()?;
        let mut w = self.wal.lock().unwrap_or_else(|p| p.into_inner());
        let lsn = w.append_checkpoint(active);
        w.sync()?;
        Ok(lsn)
    }
```

`flush`と`sync`を先に行うのがこの実装の要です。
これにより、`Checkpoint`レコードのLSNより前のすべての変更は、この時点で確実にページへ反映済みになります。
Analysisが次にWALを読むとき、`Checkpoint`より前のレコードを1件も見なくても、それらの変更がすでにPage LSNへ織り込まれていることを信頼できます。

ただし、`Checkpoint`の瞬間にActiveだったトランザクションだけは例外です。
そのトランザクションが以後1件もWALへ書かず、Checkpointの直後にクラッシュしたなら、Analysisが`Checkpoint`より後ろしか見なければ、そのトランザクションの存在にすら気づけません。
そこで`Checkpoint`レコードには、その瞬間のActiveトランザクション一覧(**Active Transaction Table**)を埋め込みます。
このエンコードは、ログレコードの形式を扱う`src/wal.rs`に置きます。

```rust
pub(crate) fn encode_active_transactions(active: &[(TransactionId, Option<Lsn>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&(active.len() as u32).to_le_bytes());
    for (txn_id, last_lsn) in active {
        buf.extend_from_slice(&txn_id.0.to_le_bytes());
        match last_lsn {
            Some(lsn) => {
                buf.push(1);
                buf.extend_from_slice(&lsn.0.to_le_bytes());
            }
            None => buf.push(0),
        }
    }
    buf
}
```

Analysisは、WALの中から最後の`Checkpoint`レコードを探し、その一覧をTransaction Tableの初期値としてから、`Checkpoint`より後ろだけを走査します。
これも`src/recovery.rs`の関数です。

```rust
fn analysis_start(records: &[LogRecord]) -> (usize, Vec<(TransactionId, Option<Lsn>)>, bool) {
    let checkpoint_index = records.iter().rposition(|r| r.record_type == LogRecordType::Checkpoint);
    match checkpoint_index {
        Some(index) => {
            let active = records[index]
                .after_image
                .as_deref()
                .and_then(decode_active_transactions)
                .unwrap_or_default();
            (index + 1, active, true)
        }
        None => (0, Vec::new(), false),
    }
}
```

SQLの`CHECKPOINT`文は、現在Activeなトランザクションをすべてこの一覧として渡すだけの薄い入口です。
通常のSQL経路の`self.tx`(高々1本)だけでなく、決定的インターリーブテストハーネス(第30章)の`harness_contexts`が同時に持ちうる複数のトランザクションも、両方ともこの一覧に含めます。
`src/ast.rs`の`CheckpointStatement`は、次のように`CHECKPOINT`文を表します。

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointStatement {
    pub span: Span,
}
```

`src/database.rs`の`execute_checkpoint`は、次のようになっています。

```rust
    fn execute_checkpoint(&mut self, _checkpoint: CheckpointStatement) -> DbResult<QueryResult> {
        let mut active: Vec<(TransactionId, Option<Lsn>)> =
            self.tx.as_ref().map(|tx| vec![(tx.id, tx.wal_last_lsn)]).unwrap_or_default();
        active.extend(self.harness_contexts.values().map(|ctx| (ctx.id, ctx.wal_last_lsn)));
        // ...
    }
```

`self.tx`と`harness_contexts`は、同じ`Database`が同時に持つ対等なActiveトランザクションの集合です(`TransactionId`の採番自体も共有しています)。
`harness_contexts`側を素通りさせると、`begin_tx`で開始したトランザクションが`INSERT`したあと`CHECKPOINT`をまたいで再起動したとき、AnalysisはそのトランザクションがCheckpointの瞬間にActiveだったことを知らないまま`Checkpoint`より後ろだけを走査します。
そのトランザクションが以後何も書かずにcrashしていれば、`Commit`も`Abort`も`Insert`もこの走査範囲に現れず、loserとして認識されないまま、未確定の行が取り消されずに残ってしまいます。

`CHECKPOINT`はAnalysisの走査範囲を短くするだけで、WALファイル自体を切り詰めません。
`Checkpoint`より前のレコードは、二度と使われないとしてもファイルに残り続けます。
ファイルを安全に切り詰めるには、「その範囲を参照しているトランザクションがもう1つも残っていない」ことをさらに確認する必要があり、この章はそこまで踏み込みません(章末の演習)。

## failpointでクラッシュを注入する

ここまでのRedoとUndoが正しく動くことは、「`flush`を呼ばずにプロセスを`drop`する」という第33章から続けてきた手口で確認できます。
けれども「Undoの途中でRecoveryそのものがもう一度クラッシュする」状況は、この手口では再現できません。
`recover`は`Storage::open`という1回の関数呼び出しの**内部**で最初から最後まで進むため、その内側で止める仕掛けが要ります。

この章は、実プロセスを本当には止めない、テスト専用のcrash point注入機構を自作します。
`src/failpoint.rs`を新しいモジュールとして作成します。
その中身は、次のとおりです。

```rust
pub fn arm(name: &'static str, count: usize) {
    ARMED.with(|cell| *cell.borrow_mut() = Some((name, count.max(1))));
}

pub(crate) fn hit(name: &'static str) -> DbResult<()> {
    ARMED.with(|cell| {
        let mut slot = cell.borrow_mut();
        let fire = match slot.as_mut() {
            Some((armed_name, remaining)) if *armed_name == name => {
                *remaining -= 1;
                *remaining == 0
            }
            _ => false,
        };
        if fire {
            *slot = None;
            return Err(DbError::Io(std::io::Error::other(format!(
                "failpoint '{name}' が発火しました(第34章のCrash Test専用の注入)"
            ))));
        }
        Ok(())
    })
}
```

あわせて`src/lib.rs`に次の宣言を加え、このモジュールを公開します。

```rust
pub mod failpoint;
```

`arm(名前, 回数)`で「この名前のfailpointが何回目に呼ばれたら失敗させるか」を予約し、`recover`の内部が要所(Redoの1レコードごと、Undoの1トランザクションごと)で`hit`を呼びます。
回数が一致すると`hit`は`Err`を返し、それがそのまま`recover`から`Storage::open`まで伝わります。

`static`ではなく`thread_local!`にしてあるのは、`cargo test`が複数のテストを別スレッドで並行に走らせるからです。
プロセス全体で共有する状態にすると、あるテストがarmした内容を別のテストが誤って踏みます。
各`#[test]`関数は既定で専用のスレッドを1本もらうため、スレッドローカルにしておけば他のテストと干渉しません。

もう1つの工夫は、`hit`が発火したら自動でarm状態を解除することです。
これにより、「1回目の`Storage::open`はN回目の直後で死ぬが、arm し直さずに呼んだ2回目は同じ場所で死なずに続行する」という、実際のクラッシュと再起動の非決定性に自然に対応します。

## クラッシュシナリオを試す

`tests/crash_recovery.rs`に、この章が主張する8つの場面をそれぞれテストとして書きます。

**(a) COMMIT応答後、データページ書き戻し前のクラッシュ**は、`BEGIN`のうちに`INSERT`を2件実行して`COMMIT`し、そのあとで`flush`せずに`drop`し、開き直した`Database`が両方の行を読めることを確認します。
`last_recovery_report()`の`records_redone`が2以上であることも合わせて確かめます。

**(b) 未COMMITの変更がページに書かれた後のクラッシュ**は、`BEGIN`のうちに`UPDATE`と`INSERT`を行い、`COMMIT`する前に`flush`を呼んでからdropします。
`flush`によって、まだ確定していない変更が実際にページへ書き戻された状況を確実に作ります。
開き直すと、`UPDATE`は元の値へ戻り、`INSERT`した行は消えています。

**(c) Undo中の再クラッシュ**は、決定的インターリーブテストハーネスで2本のトランザクションを同時にActiveにし、どちらもコミットもロールバックもしないままdropします。
`tests/crash_recovery.rs`で`failpoint::arm("recovery_undo_step", 1)`により「1本目のUndoを終えた直後」に発火するよう仕込むと、1回目の`Database::open`は確かに失敗します。

```rust
    failpoint::arm("recovery_undo_step", 1);
    assert!(Database::open(&path).is_err(), "1つ目のloserをUndoした直後に失敗するよう仕込んだ");

    // 2回目のOpenは同じ場所では失敗しない(failpointは1回発火すると
    // 自動でdisarmされる、`crate::failpoint`を参照)。
    let db = Database::open(&path).unwrap();
    let report = db.last_recovery_report().unwrap();
    assert_eq!(report.transactions_undone, 2, "2本のloserがどちらもUndoされているはず: {report:?}");
```

2回目の`Database::open`はfailpointが自動でdisarmされているため同じ場所では止まらず、2本とも最後まで取り消し切ります。

**(d) CHECKPOINT直後のクラッシュ**は、30行分`INSERT`してから`CHECKPOINT`し、その後1件だけ`INSERT`してdropします。
`last_recovery_report()`の`used_checkpoint`が`true`になり、`records_scanned`がCheckpoint後の3件程度にとどまっていることを確認します。

**(e) 何も書いていないActiveなトランザクションを含むCHECKPOINT直後のクラッシュ**は、`BEGIN`の直後、まだ1件もWALレコードを書いていないうちに`CHECKPOINT`してdropします。
このトランザクションのTransaction Table上の`last_lsn`は`None`のまま`Checkpoint`レコードへ埋め込まれるため、Undoが架空の`Lsn`を参照して`panic`しないことを確認します。

**(f) CHECKPOINTのあとで書き込みを始めるトランザクション**は、(e)と同じ`BEGIN; CHECKPOINT;`のあとに`INSERT`を1件実行してdropします。
`last_lsn`が`None`から`Some`へ切り替わる境界をまたいでも、Undoがその`INSERT`だけを正しく取り消すことを確認します。

**(g) CHECKPOINT時点でハーネスがActiveなトランザクション**は、`begin_tx`で開始したトランザクションが`INSERT`したあと、通常のSQL経路から`CHECKPOINT`してdropします。
このトランザクションが`self.tx`ではなく`harness_contexts`にあることを踏まえ、それでもTransaction Tableに含まれ、未確定の行がUndoで取り消されることを確認します。

**(h) 索引ありのRedo、Undo失敗**は、索引を持つテーブルに未確定の`INSERT`、`UPDATE`を残したままdropし、`recovery_redo_step`、`recovery_undo_step`それぞれのfailpointで1回目の`Database::open`をわざと失敗させます。
失敗の直前に読んでおいた索引ファイルのバイト列と、失敗の直後にもう一度読んだバイト列が完全に一致すること、そして2回目の`Database::open`が正しい内容で完走することの両方を確認します(本文「索引ファイルの入れ替えは一時ファイル経由」を参照)。

これらに加えて、Recovery自体の冪等性(直後にもう一度開き直しても、RedoとUndoの対象が残っていないこと)と、クラッシュ後に索引が正しく作り直されることも、それぞれ別のテストで確認しています。

```console
$ cargo test --test crash_recovery
test result: ok. 10 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## この章の限界

索引は、`Storage::open`のたびに毎回Heapから作り直します。
`CREATE INDEX`(第24章)がテーブル全体を舐めるのと同じ処理を、クラッシュしたかどうかにかかわらず起動のたびに行うため、索引を多く持つ大きなテーブルほど起動が遅くなります。
索引ページ自体をWALと結線し、索引についても位置ぴったりのRedoを行う設計は、この章では選びませんでした(第33章の限界の節を参照)。

Undoは、複数のloserトランザクションを1本ずつ直列に処理します。
本物のARIESが行う、複数loserのLSN降順インターリーブは実装していません。
シングルスレッドで動くこの教材のもとでは正しさに影響しませんが、第35章でLatchによる並行実行が入った後にこのままで良いかどうかは、この章では判断しません。

`CHECKPOINT`はAnalysisの走査範囲を短くするだけで、WALファイル自体を切り詰めません。
稼働時間が延びるほどWALファイルは肥大化し続けます。

Recoveryの物理的な書き込みは、Analysis、Redo、Undoが完全に終わるまでディスクへ一切書き戻しません。
これは途中でのクラッシュに対する安全性のためですが、裏を返せば、RedoとUndoの対象レコードが極端に多い(`BufferPool`の容量を大きく超える)場合、この前提が破れる余地が残っています。
容量を超えたぶんは`BufferPool`のClock置換がevictの形で早期に書き戻してしまう可能性があり、その場合の安全性はこの章では検証していません。

## 演習問題

### 必須課題

1. `Storage::redo_insert`の`debug_assert_eq!`は、Redoが書き込んだスロットが元のレコードの`rid.slot_id`と一致することを確認するだけで、`release`ビルドでは何も検査しません。この検査を`DbResult`を返す実際のエラー処理に置き換えると何が変わるか、そのエラーを`crate::recovery::recover`がどう扱うべきかを考えてください。
2. `crate::recovery::analysis_start`は、`Checkpoint`レコードが複数あってもLSNが最大のもの(`rposition`)しか使いません。`CHECKPOINT`を2回呼んでから`INSERT`を1件だけ実行し、そのままクラッシュさせるテストを書いて、`last_recovery_report()`の`records_scanned`が2回目の`CHECKPOINT`基準で数えられていることを確認してください。
3. `crate::failpoint`の`hit`は、armされた名前と一致しなければ何もしません。`recover`の中でこの機構を使っている2箇所(`recovery_redo_step`と`recovery_undo_step`)以外に、`crate::wal::WalWriter::sync`の直前に3つ目の注入点を追加し、「Undoが`Abort`を書き終えてsyncするその瞬間」にクラッシュさせるテストを書いてください。この章の「アトミックな`recover`」という主張が、その注入点でも成り立つか確認してください。

### 発展課題

1. 索引ページをWALと結線し、`crate::recovery::recover`が索引についても位置ぴったりのRedoを行うように設計し、実装してください(「この章の限界」を参照)。`crate::btree::BTree`の`BufferPool`に`attach_wal`を呼ぶところから始める必要があります。索引ページの変更をどう`LogRecordType`で表すか(専用の種別を増やすか、`table_id`をインデックスIDの意味で流用するか)から考えてください。
2. `CHECKPOINT`が、Checkpoint以前のWALレコードのうち、以後どのAnalysisからも参照されないものを実際にファイルから切り詰めるように拡張してください(「この章の限界」を参照)。切り詰めてよい条件(そのLSN以前を起点とするloserがもう存在しないこと)をどう判定するかから考える必要があります。
3. Undoを、複数のloserトランザクションについてLSN降順にインターリーブしながら進めるように書き換えてください(モジュールドキュメントの「ARIESを教材規模へ簡略化した点」を参照)。この教材の`apply_wal_undo_disk`は1トランザクション分の`remap`しか持たないため、複数のloserにまたがる`remap`をどう設計し直すかが課題になります。
