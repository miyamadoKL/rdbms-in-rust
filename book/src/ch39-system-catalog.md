# 第39章 System Catalogとメンテナンス

`orders`というテーブルにどんな列があったか、思い出せなくなったとしましょう。
`amount`だったか`status`だったか自信が持てず、とりあえず打ってみます。

```console
minidb> SELECT status FROM orders;
エラー: 行1列8: 名前解決エラー: 列'status'が見つかりません
```

列が存在しないことは分かりましたが、では実際にはどんな列があるのかは、このエラーメッセージからは分かりません。
`id`、`amount`、あるいはまったく別の名前かもしれません。
列名を1つずつ`SELECT`で試し、エラーにならない名前を探り当てていくのでしょうか。

psqlを使ったことがあれば、`\d orders`と打てば列名、型、索引が一覧で返ってくることを知っているはずです。
第38章までの`minidb`に、それに相当するコマンドはありません。
テーブルの中身を覗く手段は、結局のところ`SELECT`だけです。

## SQLでは覗けないデータベースの中身

もう1つ、覗けないものがあります。
削除したはずの行が、ファイルの中でどうなっているかです。

```console
minidb> CREATE TABLE logs (id BIGINT NOT NULL, payload TEXT NOT NULL);
CREATE TABLE
minidb> -- 400バイトの行を600件挿入しては、そのほとんどをDELETEする、を5回繰り返す
minidb> SELECT COUNT(*) FROM logs;
COUNT(*)
--------
50
(1 row)
```

5回の挿入と削除を終えた時点で、テーブルに残っている行はたった50件です。
けれどこのテーブルが実際に使っているデータページの枚数を測ってみると(測り方は本文「実測: VACUUMの有無でファイルサイズはどう変わるか」を参照)、1サイクルごとに約67ページずつ増え続け、5サイクル目には334ページに達しています。
50行しか残っていないテーブルが、334ページぶんのディスク領域を占有したままです。

原因は、第12章の`SlottedPage::delete`と第24章のB+Tree`delete`が、どちらも**Lazy Delete**という設計を選んでいたことにあります。
`SlottedPage::delete`はスロットを`Tombstone`(墓標)に変えるだけで、タプルのバイト列自体は消しません。
その死んだバイト列を実際に空き領域へ回収するのは`SlottedPage::compact`の仕事ですが、`compact`は空き領域が足りずに`insert`が詰まったときにだけ自動で呼ばれます(第12章)。
B+Treeの`delete`も同じです。
Leaf Pageからエントリを取り除くだけで、隣接ページ間でエントリを融通し合うRedistributionも、ページ同士をまとめるMergeも行いません(第24章)。
`DELETE`を繰り返しても、ページの中の隙間は増える一方で、ページの枚数そのものは決して減りません。

Lazy Deleteそのものは、この教材が意図して選んだ単純化です。
`compact`や`Redistribution`、`Merge`を`delete`の実行中に毎回行うと、削除1件あたりのコストが跳ね上がります。
けれど、その後始末をいつまでも先送りにできるわけではありません。
この章は、System TableでSQLからデータベースの中身を覗けるようにしたうえで、先送りにしてきた後始末を`VACUUM`という1つの文にまとめて片づけます。

## SHOW TABLESとDESCRIBE: System TableをSQLへ橋渡しする

psqlの`\d`はメタコマンドであり、SQLではありません。
クライアント側(`psql`自身)がサーバーへ`information_schema`や`pg_catalog`という**実テーブル**へのSELECT文を投げ、その結果を整形して表示しています。
PostgreSQLのSystem Catalogは、テーブル定義、列定義、索引定義それぞれが、他のテーブルと何ら変わらない行として`pg_class`、`pg_attribute`、`pg_index`に格納されています。

この章はその方式を採りません。
`SHOW TABLES`、`DESCRIBE <table>`、`SHOW INDEXES`、`SHOW STATS`という4つの専用構文を追加し、それぞれの実行結果を`src/database.rs`で`QueryResult`として直接組み立てます。

```rust
/// `SHOW TABLES`・`DESCRIBE`・`SHOW INDEXES`・`SHOW STATS`(第39章)が
/// 完了したことを表す`QueryResult`を作る。
fn table(schema: Schema, rows: Vec<Tuple>) -> Self {
    QueryResult { schema, rows, command_tag: None }
}
```

`Catalog`(第9章)や`Storage`(第15章)がすでにメモリ上に持っている`TableInfo`、`Schema`、`IndexInfo`を、そのまま`Schema`と`Vec<Tuple>`の組へ詰め替えるだけです。
仮想テーブル方式であれば`pg_class`相当のテーブルを`Binder`、`physical_plan`(第17〜19章)に通常のテーブルと同じ経路で読ませる必要があり、`WHERE`や`JOIN`を素通りさせるための配線が新たに要ります。
`EXPLAIN`(第19章)がすでに同じ方式(`PhysicalPlan`の木を文字列化し、1行1タプルの`QueryResult`として返す)を採っており、この章の4文もその前例を踏襲しました。
この選択の代償は、`SHOW TABLES`の結果を`WHERE table_name LIKE '%log%'`のように絞り込めないことです。
そのぶん、実装は`Backend::Memory`、`Backend::Disk`のどちらのメタ情報にも直接アクセスするだけの単純な形に収まっています。

`SHOW TABLES`は、テーブル名、行数概算、ページ数の3列を返します。
`orders`を、今度は`PRIMARY KEY`と索引つきで作り直して確かめます。

```console
minidb> CREATE TABLE orders (id BIGINT NOT NULL PRIMARY KEY, customer_id BIGINT NOT NULL, amount BIGINT NOT NULL);
CREATE TABLE
minidb> CREATE INDEX orders_customer_idx ON orders (customer_id);
CREATE INDEX
minidb> INSERT INTO orders VALUES (1, 100, 500), (2, 100, 300), (3, 200, 900);
INSERT 3
minidb> SHOW TABLES;
table_name | rows | pages
-------------------------
orders | NULL | 1
(1 row)
```

行数の列が`NULL`なのは、このテーブルにまだ`ANALYZE`(第27章)を1度も実行していないからです。
`Backend::Disk`の行数は`ANALYZE`が最後に集計した`row_count`をそのまま表示する、あくまで「最後に集計した時点のスナップショット」です。
`INSERT`、`DELETE`のたびに数え直すことはしません。
`Storage::table_page_count`(第28章、コストモデルがすでに使っていた実測値)がその都度の実際のページ枚数を返すのとは対照的です。
`Backend::Memory`は`ANALYZE`を経由せず、`MemStorage`が持つ`Vec<Tuple>`の長さをその場で数え直すため、常に正確な行数を返します(メモリ上の`Vec`の長さを読むだけなので、数え直しても新たなディスクI/Oは発生しません)。
2つのバックエンドで行数の性質が違うのは意図的な設計であり、`Backend::Memory`にはページという概念自体が無いため`pages`列は常に`NULL`です。

`DESCRIBE`は、列名、型、`NOT NULL`、`PRIMARY KEY`、`UNIQUE`、その列を対象にした索引名を返します。

```console
minidb> DESCRIBE orders;
column_name | data_type | not_null | primary_key | unique | indexes
-------------------------------------------------------------------
id | BIGINT | true | true | false | orders_id_idx
customer_id | BIGINT | true | false | false | orders_customer_idx
amount | BIGINT | true | false | false |
(3 rows)
```

`id`列の`orders_id_idx`は、`PRIMARY KEY`に対応して自動生成された制約索引です(第20章、第24章の`create_table_with_constraint_indexes`)。
`customer_id`列の`orders_customer_idx`は、この章より前に`CREATE INDEX`で作った索引です。
どちらも`Storage::indexes_for_table`(第24章)が返す`IndexInfo`の`column_index`を、`DESCRIBE`の側で列ごとに突き合わせているだけで、専用の対応表を新しく持つ必要はありませんでした。

## SHOW INDEXESとSHOW STATS: 索引と統計の可視化

`SHOW INDEXES`は、索引名、対象テーブル、対象列、`UNIQUE`、`PRIMARY KEY`に加えて、その索引が何回引かれたかという利用回数を返します。

```console
minidb> SHOW INDEXES;
index_name | table_name | column_name | unique | primary_key | uses
-------------------------------------------------------------------
orders_customer_idx | orders | customer_id | false | false | 0
orders_id_idx | orders | id | true | true | 0
(2 rows)
```

`uses`列は、`src/storage.rs`の`Storage`に新しく持たせた1つのカウンタが支えています。

```rust
/// 索引名ごとの`BTree::lookup`・`BTree::range`の呼び出し回数(第39章、
/// `SHOW STATS`が表示する)。
index_usage: RefCell<HashMap<String, u64>>,
```

`&mut Storage`ではなく`&Storage`のまま増やせる必要があるのは、この値を増やす場所が`IndexScanExec`、`IndexNestedLoopJoinExec`(第25章)という、`Storage`を`&'a Storage`としてしか借用していない`Executor`の内部だからです。
`RefCell`による内部可変性を使い、`BTree::lookup`、`BTree::range`を呼ぶ直前にそれぞれ1箇所ずつ、`src/physical_plan.rs`の`IndexScanExec`と`IndexNestedLoopJoinExec`で記録します。

```rust
storage.record_index_use(index_name);
let source = match kind {
    IndexScanKind::Point(value) => IndexScanSource::Point(btree.lookup(value)?.into_iter()),
    IndexScanKind::Range { lower, upper } => IndexScanSource::Range(btree.range(lower.as_ref(), upper.as_ref())?),
};
```

`SHOW STATS FROM <table>`は、`ANALYZE`(第27章)が集めた列ごとの統計を可視化します。

```console
minidb> ANALYZE orders;
ANALYZE 1
minidb> SHOW STATS FROM orders;
column_name | null_count | distinct_count | min | max | mcv_count | histogram_buckets
-------------------------------------------------------------------------------------
id | 0 | 3 | 1 | 3 | 0 | 3
customer_id | 0 | 2 | 100 | 200 | 1 | 1
amount | 0 | 3 | 300 | 900 | 0 | 3
(3 rows)
```

これまで統計情報は`EXPLAIN`の`rows=`という1個の数字を通してしか観測できませんでした。
`mcv_count`、`histogram_buckets`という列は、`ColumnStats`の`mcv`、`histogram`(第27章)の**長さ**をそのまま表示しています。
`customer_id`の`mcv_count`が`1`なのは、`100`が2回、`200`が1回という分布で、`100`のほうが平均バケツ行数を上回りMCVへ移った結果です(`extract_mcv`の固定点抽出、第27章)。
`ANALYZE`を1度も実行していないテーブルを指定すると、空の結果を黙って返す代わりに`src/database.rs`でエラーにします。

```rust
let stats = self.table_stats(info.id).ok_or_else(|| DbError::TableNotAnalyzed(table_name.to_string()))?;
```

集計していない統計情報を空の表として見せてしまうと、「集計した結果0件だった」のか「まだ集計していない」のかが利用者から区別できません。

## VACUUM: 遅延した削除の後始末

`VACUUM [テーブル名]`は、この章がここまで温存してきたLazy Deleteの後始末を1つの文にまとめます。
テーブル名を省略すると、カタログに登録されている全テーブルが対象になります。
`src/storage.rs`の`Storage::vacuum_table`が、対象テーブル1つぶんの回収を担います。

```rust
pub fn vacuum_table(&mut self, table_id: TableId) -> DbResult<VacuumReport> {
    let page_ids = self.table_entry(table_id)?.page_ids.clone();
    let mut reclaimed_pages = 0usize;
    let mut remaining = Vec::with_capacity(page_ids.len());
    for page_id in page_ids {
        let mut guard = self.pool.write_page(page_id)?;
        let mut page = SlottedPage::open(guard.data_mut())?;
        page.compact();
        let is_empty = page.is_empty();
        let free = page.free_space();
        drop(guard);
        if is_empty {
            self.fsm.remove(page_id);
            self.free_pages.push(page_id);
            reclaimed_pages += 1;
        } else {
            self.fsm.update(page_id, free);
            remaining.push(page_id);
        }
    }
    self.tables.get_mut(&table_id).expect("直前にtable_entryで存在を確認済み").page_ids = remaining;

    let index_names: Vec<String> =
        self.indexes.values().filter(|e| e.info.table_id == table_id).map(|e| e.info.name.clone()).collect();
    let rebuilt_indexes = index_names.len();
    for index_name in &index_names {
        let (temp_path, index_path) = self.rebuild_one_index_after_recovery(index_name)?;
        std::fs::rename(&temp_path, &index_path)?;
    }

    self.persist_catalog()?;
    Ok(VacuumReport { reclaimed_pages, rebuilt_indexes })
}
```

やっていることは3段階です。

1. テーブルが使っている各ページを`SlottedPage::compact`(第12章)し、Tombstoneの死んだバイト列を回収する。
2. `compact`後にOccupiedなスロットが1つも残っていないページを、`Storage::drop_table`(第15章)がテーブル削除時に行うのと同じ手順でFree Page Listへ返す。
3. このテーブルに対応する全索引を作り直し、Lazy Delete済みのエントリを含まないB+Treeへ入れ替える。

### 空ページの判定には新しいメソッドが1つだけ要る

1段階目と2段階目の境目にあるのが、`src/slotted_page.rs`に加える`SlottedPage::is_empty`という新しいメソッドです。

```rust
/// Occupiedなスロットが1つも無いかどうか(第39章、`VACUUM`)。
pub fn is_empty(&self) -> bool {
    let (slot_count, _) = self.header();
    (0..slot_count).all(|i| !matches!(self.slot_entry(SlotId(i)), Some((_, _, STATUS_OCCUPIED))))
}
```

`compact`はSlot Directory自体(スロット数、Tombstoneのままのエントリ)には触れません(第12章)。
そのため、ページの中身が空になったかどうかは、Tuple Data領域の空きバイト数(`free_space`)ではなく、Slot Directoryを1件ずつ見て回ってどのスロットもOccupiedでないことを確かめる必要があります。

### B+Treeの索引再構築は、Crash Recoveryの機構をそのまま借りる

3段階目、索引の作り直しには新しいコードをほとんど書いていません。
第34章のCrash Recoveryが、索引ページをWALと結線していないという限界の埋め合わせとして実装していた`rebuild_one_index_after_recovery`をそのまま呼んでいます。
この関数は、対象の索引が持つ`(key, RecordId)`の対応を`Storage::scan`でHeapから読み直し、同じディレクトリの一時ファイルへ新しいB+Treeとして組み立て、その一時ファイルのパスを返すだけです。
`VACUUM`はこの一時ファイルを`rename`で実際の索引ファイルへ差し替えます。

B+TreeのDeleteは第24章からLazy Deleteのままで、Redistribution、Mergeという物理的な回収手段を持ちません。
削除を繰り返した索引の高さは、放っておけば二度と縮みません(第24章の`delete_does_not_shrink_the_tree_height`が確認しているとおりです)。
この章は、その物理的な回収を「差分だけ取り除く」のではなく「生きているエントリだけで丸ごと作り直す」という設計で解決しました。
Redistribution、Mergeを実装する道も検討しましたが、隣接ページの特定、エントリの移動、親の分離キーの更新という一連の手続きは、第23、24章のSplitの逆操作を丸ごと書き起こすのに等しい分量になります。
すでに動いている「Heapから作り直す」機構を1回呼ぶだけで済むこの設計のほうが、この章の分量に見合っています。

### VACUUMの排他

`VACUUM`が物理的にページを動かしている間、他のトランザクションが同じテーブルの行を書き換えていたらどうなるでしょうか。
この章は、`SELECT`、`UPDATE`、`DELETE`(第31章)がすでに使っているLock Managerへ、`src/database.rs`からそのまま相乗りします。

```rust
for &table_id in &targets {
    self.acquire_scan_locks(owner, &[table_id], LockMode::Exclusive)?;
}
```

`acquire_scan_locks`は、`Backend::Disk`ではその時点でテーブルに存在する行の`RecordId`すべてに`LockKey::Tuple`のロックを掛けるヘルパーです(第31章、`SELECT`がShared版を使っています)。
`VACUUM`はこれを`Exclusive`で呼ぶことで、テーブル全体を書き込みロックしたのと同じ効果を得ます。

単純に`LockKey::Table(table_id)`という1個のロックを取るだけでは足りません。
`Backend::Disk`の`UPDATE`、`DELETE`は、`WHERE`に一致した行だけに`LockKey::Tuple`のExclusiveを掛けるという行単位の粒度で動いています(第31章、`crate::database`モジュール冒頭「ロックの粒度」を参照)。
`LockKey::Table`と`LockKey::Tuple`はLock Manager上ではまったく別の鍵であり、片方だけをロックしても、もう片方の粒度で動いている文とは衝突しません。
テーブル全体の行1つ1つに`Exclusive`を掛けることで初めて、進行中の`UPDATE`、`DELETE`が持つ行ロックと確実にぶつかります。

ロックが獲得できなければ、他の文とまったく同じ`DbError::WouldBlock`が返ります。
`SharedDatabase::execute_in_tx`(第35章)の再試行ループが、この値を受け取ったスレッドを実際に眠らせ、ロックを持つトランザクションがCOMMIT、ROLLBACKするたびに起こして`VACUUM`を再試行します。
`VACUUM`のために新しい待機の仕組みを1つも足していません。

```console
minidb> BEGIN;
minidb> UPDATE orders SET amount = amount + 1 WHERE id = 1;
```

この状態で別の接続から`VACUUM orders`を実行すると、`id = 1`の行に対する`UPDATE`のExclusiveロックとぶつかり、そのトランザクションが`COMMIT`または`ROLLBACK`するまで待たされます。

### WALの切り詰めは、今回も演習に残す

第34章は、`CHECKPOINT`がAnalysisの走査範囲を短くするだけで、WALファイル自体は切り詰めないという限界を残していました。
稼働時間が延びるほどWALファイルは肥大化し続けます。
この章はその切り詰めを`VACUUM`に含めるかどうかを検討しましたが、含めないことにしました。
`CHECKPOINT`より前のレコードを安全に切り詰めるには、「そのLSNより前を起点とするloserがもう存在しない」ことをAnalysisの走査結果とは別に確認する仕組みが要り、`VACUUM`のページ、索引の回収とは性質の異なる作業です。
`VACUUM`はデータページと索引ファイルの回収に的を絞り、WALの切り詰めは第34章から引き続き演習課題として残します。

## 実測: VACUUMの有無でファイルサイズはどう変わるか

400バイトの行を600件挿入しては、直近50件を残してほとんどを`DELETE`する、というサイクルを5回繰り返します。
`VACUUM`を挟まない場合と、サイクルごとに`VACUUM logs`を挟む場合とで、同じワークロードを比較します。

```console
$ cargo test --release --lib -- --ignored --nocapture without_vacuum_the_file_keeps_growing_while_with_vacuum_it_plateaus
without VACUUM: pages=67
without VACUUM: pages=134
without VACUUM: pages=200
without VACUUM: pages=267
without VACUUM: pages=334
without VACUUM: file_size=1376256bytes
with VACUUM: pages=6
with VACUUM: pages=7
with VACUUM: pages=7
with VACUUM: pages=7
with VACUUM: pages=7
with VACUUM: file_size=307200bytes
```

`VACUUM`を挟まない側は、サイクルを重ねるたびに使用ページが約67ページずつ増え続けます。
5サイクル目には334ページ、ファイルサイズは約1.3MiBに達しています。
テーブルに残っている行数はどちらの実験でも常に50件のままなので、この増加はすべて回収されないTombstoneの積み重ねです。

`VACUUM`を挟む側は、1サイクル目で6ページまで落ち着いたあと、以後は7ページで頭打ちになります。
最終的なファイルサイズは約300KiBで、`VACUUM`を挟まない側の4分の1以下です。
`Storage::insert`は空きページをまず`FreeSpaceMap`、次にFree Page Listから探します(第15章)。
`VACUUM`が空にしたページはFree Page Listに積まれているため、次のサイクルの`INSERT`はファイルを伸ばさずにそのページへ書き込めます。

なお、`VACUUM`を実行した直後にファイルそのものが縮むわけではありません。
このクレートの`DiskManager`(第13章)はページを解放する手段を持たず、ファイルは一度伸びると縮みません。
`VACUUM`が行うのは、空になったページを**再利用可能な状態**にすることだけです。
それでも、上の実測が示すとおり、この再利用だけで「INSERT/DELETEを繰り返すほど一方的に太り続けるファイル」を「ある大きさで頭打ちになるファイル」に変えるには十分です。

## 観測性: Buffer Pool、索引利用回数、Query Timing

`SHOW STATS`は、テーブル名を省略するとエンジン全体の運用統計を返します。

```console
minidb> SHOW STATS;
metric | value
--------------
buffer_pool_hits | 7
buffer_pool_misses | 2
buffer_pool_hit_rate_pct | 77.8
query_count | 8
query_avg_ms | 1.778
index_uses:orders_customer_idx | 0
index_uses:orders_id_idx | 0
(7 rows)
```

`metric`、`value`という2列の形式を選んだのは、Buffer Poolのヒット率、索引ごとの利用回数、Query Timingという性質の異なる指標を、テーブルの列数を増やさずに同じ結果へ積み増せるからです。
索引が1本増えても`SHOW STATS`の列構成を変える必要はなく、`index_uses:<索引名>`という行が1つ増えるだけです。

Buffer Poolのヒット率は、第14章から存在していた`BufferPool::stats`を`src/storage.rs`でそのまま公開しただけです。

```rust
pub fn buffer_pool_stats(&self) -> BufferPoolStats {
    self.pool.stats()
}
```

Query Timingは、`src/database.rs`にこの章で新しく足した唯一のカウンタです。

```rust
struct QueryTimingStats {
    count: u64,
    total: std::time::Duration,
}
```

`Database::run_bound_statement`(束縛済みの文を実際に実行する共通の本体、第31章から`lock_owner`とともにこの形です)が、文を1本実行するたびに`Instant::now()`から`elapsed()`までを足し込みます。

```rust
fn run_bound_statement(&mut self, bound: BoundStatement, ctx: &ExecutionContext) -> DbResult<QueryResult> {
    let owner = self.lock_owner();
    let started = std::time::Instant::now();
    let result = match bound {
        // ...
    };
    // Query Timing(第39章、`SHOW STATS`)。文の成否を問わず数える
    // (失敗した文もエンジンが時間を使ったことに変わりは無い)。
    self.query_timing.record(started.elapsed());
    if self.tx.is_none() {
        self.lock_manager.release_all(owner);
    }
    result
}
```

`ExecutionContext`(第38章)のキャンセル・タイムアウトが特定の実行経路(`Session`経由の文)にしか及ばなかったのとは違い、Query Timingは`run_bound_statement`という最も内側の共通点に置いたことで、埋め込み用途の`Database::execute`、`Session::execute`、決定的インターリーブテストハーネス(第30章)のどの経路を通った文も等しく数えます。

## Slow Query Log

Query Timingが累積の平均を返すのに対し、Slow Query Logは個々の遅い文を名指しします。
サーバー起動時に`--slow-query-threshold-ms`を指定すると、その閾値を超えた文をSQL文、実行時間、行数つきでstderrへ記録します。
この章はこの仕組みのために`src/slow_query_log.rs`を新規に作成し、`src/lib.rs`へ`pub mod slow_query_log;`を追加します。

サーバー側の端末です。

```console
$ cargo run -- --serve 127.0.0.1:5432 example.db --slow-query-threshold-ms 0
minidb: 127.0.0.1:5432で接続を待機しています(ワーカー16本、キュー64件)
[slow query] 0.086ms rows=0 outcome=ok sql=CREATE TABLE t (id BIGINT NOT NULL);
[slow query] 16.815ms rows=0 outcome=ok sql=INSERT INTO t VALUES (1);
```

別の端末から`minidb-client`(第36章)で接続し、`CREATE TABLE`と`INSERT`を1文ずつ実行すると、サーバー側の端末に上のログが1行ずつ現れます。

```console
$ cargo run --bin minidb-client -- 127.0.0.1:5432
minidb> CREATE TABLE t (id BIGINT NOT NULL);
CREATE TABLE
minidb> INSERT INTO t VALUES (1);
INSERT 1
```

閾値を`0`に設定しているので、すべての文が記録されています。
`INSERT`が`CREATE TABLE`よりずっと長い16.815msかかっているのは、`INSERT`が`COMMIT`に伴うWALの`sync`(第33章、ディスクへの同期書き込み)を待つのに対し、`CREATE TABLE`はメタデータの更新だけで完結するためです。

記録先をログファイルではなくstderrに決めたのは、ログローテーションのようなファイル運用をこの教材の範囲に持ち込まず、「標準エラー出力をどこへ流すか」を利用者(`systemd`のジャーナル、シェルのリダイレクト)へ委ねられるからです。
呼び出し箇所は2つあります。
`Database::execute_bound_statement`(REPL、埋め込み用途、大半のテストが使う低レベルAPI)と、`Session::execute`(第37章、Server、REPLが実際に使う経路)です。
この2つは同じSQL文字列に対して同時に呼ばれることのない、独立した実行経路です(`Session`は`Database::execute_bound_statement`を経由しません)。
どちらの経路も最終的に呼ぶ`src/slow_query_log.rs`の`log_to`は、次のとおりです。

```rust
fn log_to(out: &mut impl Write, threshold: Option<Duration>, sql: &str, elapsed: Duration, result: &DbResult<QueryResult>) {
    let Some(threshold) = threshold else { return };
    if elapsed < threshold {
        return;
    }
    let rows = result.as_ref().map(|r| r.rows().len()).unwrap_or(0);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    let _ = writeln!(out, "[slow query] {:.3}ms rows={rows} outcome={outcome} sql={}", elapsed.as_secs_f64() * 1000.0, sql.trim());
}
```

書き込み先を`impl Write`として受け取れるようにしてあるのは、実プロセスのstderrを奪い合わずにテストが出力内容を確認できるようにするためです。
本番用の`maybe_log`はこの`log_to`を`std::io::stderr()`で呼ぶだけの薄いラッパーです。

`EXECUTE name`で実行される`PREPARE`済みの文は、`PREPARE`時のSQL文ではなく`EXECUTE name`という呼び出し文字列自体では記録しません(この章の限界節を参照)。

## テスト

`src/slotted_page.rs`の単体テストは、`SlottedPage::is_empty`が「スロットが1つも無い」、「一部だけTombstone化」、「全スロットTombstone化」の3状態を正しく判定することを確認します。

`src/storage.rs`の単体テストは、`Storage::vacuum_table`を次の観点で確認します。

- 全行を削除したテーブルは、`VACUUM`で全ページがFree Page Listへ回収される
- 生きている行が1つでも残るページは回収されない
- `VacuumReport::rebuilt_indexes`が、テーブルに対応する索引の本数と一致する
- 索引利用回数(`index_usage_counts`)は`record_index_use`で増え、`DROP INDEX`で消える
- Buffer Poolのヒット/ミス統計(`buffer_pool_stats`)が実際の読み込みを反映する

`src/database.rs`の単体テストは、SQL経由での挙動を確認します。

- `SHOW TABLES`、`DESCRIBE`、`SHOW INDEXES`、`SHOW STATS`の出力列と値
- `SHOW STATS FROM <table>`は`ANALYZE`前だと`DbError::TableNotAnalyzed`
- `VACUUM`後、`SELECT`が正しい行を返し続ける(索引の`Point`、`Range`検索を含む)
- `VACUUM`は、他のトランザクションが行ロックを持っている間`DbError::WouldBlock`を返し、そのトランザクションがCOMMITすれば実行できる
- `Backend::Memory`に対する`SHOW INDEXES`、`SHOW STATS`(テーブル指定なし)、`VACUUM`は`DbError::NotImplemented`

`src/slow_query_log.rs`の単体テストは、書き込み先を`Vec<u8>`に差し替えた`log_to`を直接呼び、次を確認します。

- 実行時間が閾値以上なら記録し、未満なら記録しない
- 閾値が`None`(未設定)なら記録しない
- 失敗した文は`rows=0`、`outcome=error`として記録する

「INSERT/DELETEを繰り返してもファイルが頭打ちになる」という本文の主張そのものは、`#[ignore]`付きの実測テスト(本文「実測」節)で確認します。
既存のテストは全章にわたって回帰していません。

## この章の限界

`VACUUM`は、対象テーブルの現存する全行にExclusiveロックを掛けます。
Full Tableロックに近い荒い粒度であり、`VACUUM`の実行中は他のトランザクションがそのテーブルの行を一切書き換えられません。
PostgreSQLの通常の`VACUUM`(`VACUUM FULL`ではないほう)は、他のトランザクションと同時に実行できるよう、はるかに細かい制御を行っています。

WALファイルの切り詰めは、この章でも実装しませんでした(第34章から続く限界です)。

B+Treeの再構築は、`std::fs::rename`による索引ごとの即座の置き換えです。
Crash Recovery(第34章)が全索引ぶんの`rename`をAnalysis、Redo、Undoの完了後にまとめて行うのとは違い、`VACUUM`はクラッシュ安全性を主張しません。
複数の索引を持つテーブルの`VACUUM`中にI/Oエラーが起きた場合、すでに作り直し終えた索引と、まだ手つかずの索引が混在した状態で処理が止まります。

Slow Query Logは、`EXECUTE name`で実行される`PREPARE`済みの文を、その`EXECUTE name`という呼び出し文字列のまま記録します。
`PREPARE`時の元のSQL文とは対応づけません。

`SHOW TABLES`、`SHOW INDEXES`、`SHOW STATS`は専用構文であり、`WHERE`で絞り込んだり`JOIN`したりはできません。

## 到達点

`SHOW TABLES`、`DESCRIBE`、`SHOW INDEXES`、`SHOW STATS`が、カタログ、統計情報、Buffer Pool、索引利用回数をSQLから覗けるようにしました。
`VACUUM`が、Tombstone化されたタプルの死んだバイト列、空になったページ、Lazy Delete済みの索引エントリを回収し、INSERT/DELETEを繰り返してもファイルが一方的に太り続けることはなくなりました。
Query TimingとSlow Query Logが、エンジンがどれだけの時間を使っているか、どの文が遅いかを、実行中のプロセスの外から観測できるようにしました。
`SELECT`だけでは覗けなかったデータベースの中身は、この章でようやくSQLの手の届く範囲に入りました。

## 演習問題

### 必須課題

1. `Storage::vacuum_table`は、複数の索引を持つテーブルで2本目以降の索引を作り直している最中にI/Oエラーが起きると、すでに`rename`済みの索引と手つかずの索引が混在したまま処理を終えます(本文「この章の限界」を参照)。`crate::failpoint`(第34章)を使って、2本の索引を持つテーブルの`VACUUM`中に1本目の`rename`直後で失敗させるテストを書き、実際にこの混在状態が起きることを確認してください。
2. `acquire_scan_locks(owner, &[table_id], LockMode::Exclusive)`ではなく、単純な`LockKey::Table(table_id)`だけを取る実装に変えると何が起きるか、本文「VACUUMの排他」で説明した`UPDATE`との衝突が実際に起きなくなることをテストで再現してください。
3. `SHOW STATS`(テーブル指定なし)の`query_avg_ms`は、`Database::run_bound_statement`を通過したすべての文(`BEGIN`、`COMMIT`、`ROLLBACK`、`CHECKPOINT`を除く)を等しく数えています。`EXPLAIN`だけを100回実行してから`SHOW STATS`を見ると、`query_avg_ms`がどう変化するか実測し、この指標が「重い文」と「軽い文」を区別しない粗さを持つことを確認してください。

### 発展課題

1. PostgreSQLに倣い、`VACUUM`の対象テーブルへの読み書きを完全に止めない設計を検討してください。`SlottedPage::compact`をページ単位のLatch(第35章)だけで保護し、テーブル全体のロックを取らずに1ページずつ回収する方式が考えられます。この方式で、`compact`の最中に別のトランザクションが同じページへ`INSERT`しようとした場合の扱いを設計し、実装してください。
2. WALファイルの切り詰めを実装してください(第34章、この章の両方が演習送りにした課題)。`CHECKPOINT`以前のレコードのうち、以後どのAnalysisからも参照されない範囲を安全に判定する方法から設計する必要があります。
3. `EXECUTE name`で実行される`PREPARE`済みの文を、`PREPARE`時の元のSQL文で記録するようSlow Query Logを拡張してください。`Session`が`PreparedStatement`に元のSQL文字列を持たせ、`execute_prepared`(第37章)から`slow_query_log::maybe_log`へ渡す経路を設計してください。
