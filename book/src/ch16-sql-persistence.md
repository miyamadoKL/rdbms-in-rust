# 第16章 SQL経路の永続化

前章の`Storage`は、`create_table`、`insert`、`scan`をSQLを介さず直接呼ぶテストで、テーブル定義と行の両方が再起動をまたいで残ることをすでに確認しています。
ところが、この時点の`minidb`をビルドして`cargo run`で動かすと、次のようになります。

```console
$ cargo run
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> INSERT INTO users VALUES (1, 'Alice');
INSERT 1
minidb> \q
$ cargo run
minidb> SELECT * FROM users;
エラー: テーブルが存在しません: users
```

`Storage`はディスクに書けているのに、`users`が消えています。
原因は`Storage`の側ではありません。

`Database::execute`が`CREATE TABLE`や`INSERT`を受け取ったとき、前章までの実装が実際に書き込んでいたのは`Catalog`(第9章)と`MemStorage`(第10章)、つまりプロセスのメモリ上だけの入れ物でした。
REPL(`main.rs`)は`Database::memory()`しか呼んでいないため、`\q`でプロセスが終了した瞬間、そこに積んだ`users`もろとも消えます。
`Storage`という永続化の仕組みそのものは前章で完成していますが、SQLを実行する経路がまだそれを一度も呼んでいません。
`CREATE TABLE`、`INSERT`、`SELECT`、`UPDATE`、`DELETE`という5つの文の実行部分を、`Catalog`と`MemStorage`から`Storage`へつなぎ直すことが、この章の仕事です。

## 行の供給源をどう切り替えるか

`Database`にとって、テーブル定義と行をメモリ上に置くかディスク上に置くかは、これから先も両方使い続ける選択です。
`Database::memory()`は第1部からの読者向けの入口としても、後続の章の単体テストの土台としても、この先ずっと使い続けます。
つまり`Database`は、1つの型のまま2種類の裏側を持ち続ける必要があります。

```rust
enum Backend {
    Memory { catalog: Catalog, storage: MemStorage },
    Disk { storage: Storage },
}

pub struct Database {
    functions: FunctionRegistry,
    backend: Backend,
}
```

`trait`でこの2つを抽象化する案も検討しました。
`Backend`をtraitにして`Memory`用と`Disk`用の実装をそれぞれ用意すれば、`execute_*`側の分岐は消えます。
しかし、この2つの実装が入れ替わるのは`Database::memory()`か`Database::open()`かで起動時に1回決まるときだけで、実行中に動的に差し替わることはありません。
差し替わらない選択に対して動的ディスパッチや型引数を持ち込んでも、実行時のコストと引き換えに得られるものがなく、`match`で2つの実装を並べて読み比べられる`enum`のほうが、この章の分量では見通しが良いと判断しました。

もう1つ、`Memory`の内側は`Catalog`と`MemStorage`という2つの部品のままにしてあります。
`Storage`のように両方を1つの構造体へまとめ直す案も考えられますが、そうすると前章までの`Catalog`、`MemStorage`のテスト、そして第9章と第10章の本文で示したコードそのものが、この章の都合で書き換わってしまいます。
`Memory`側は前章までの2つの部品をそのまま`enum`の1つのバリアントに収め、`Disk`側だけが新しい`Storage`を持つという非対称を、素直に残すことにしました。

`execute_*`の各関数からこの`enum`を使うために、テーブル名からの解決だけを一箇所にまとめておきます。

```rust
fn table_info(&self, name: &str) -> Option<&TableInfo> {
    match &self.backend {
        Backend::Memory { catalog, .. } => catalog.table(name),
        Backend::Disk { storage } => storage.table(name),
    }
}
```

`Catalog::table`と`Storage::table`は、どちらも第9章で定義した同じ`TableInfo`型を返します。
`Storage`が独自の永続カタログ(第15章)を持ちながら、テーブル1件の情報を表す型としては`Catalog`と同じものを使い回しているおかげで、`table_info`はバックエンドの違いを1箇所の`match`に閉じ込めるだけで済みます。

## CREATE TABLEとDROP TABLEをつなぐ

`CREATE TABLE`は、列定義を`Schema`へ組み立てる部分(列名の重複検査、型名の解決)を前章までそのまま使い、登録先だけをバックエンドで分けます。

```rust
let schema = Schema::new(columns);
match &mut self.backend {
    Backend::Memory { catalog, storage } => {
        let id = catalog.create_table(&create.table.name, schema)?;
        storage.create_table(id);
    }
    Backend::Disk { storage } => {
        storage.create_table(&create.table.name, schema)?;
    }
}
Ok(QueryResult::command("CREATE TABLE"))
```

`Memory`側は`Catalog::create_table`で名前と`Schema`の対応を登録してから、`MemStorage::create_table`で空の行の入れ物を別に作ります(第9章と第10章)。
`Disk`側は`Storage::create_table`1回の呼び出しで済みます。
`Storage`はテーブル定義と`page_ids`をすでに1つのエントリとして持っているため(第15章)、`Database`の側で2段階に分ける理由がありません。
`DROP TABLE`も同じ形で、`Catalog::drop_table`+`MemStorage::drop_table`の2手か、`Storage::drop_table`の1手かに分かれます。

## SELECTの供給源を差し替える

`FROM`を伴う`SELECT`は、Sequential Scan→(あれば)Filter→Projectionという順序で`executor`の演算子を適用します(第10章)。
このうち、行を実際に読み出すSequential Scanだけがバックエンドごとに実装を持ち、Filter(`executor::filter`)とProjection(`executor::project`)は前章までのコードを1行も変えていません。

```rust
let scanned = match &self.backend {
    Backend::Memory { storage, .. } => {
        let mem_table = storage
            .table(table_info.id)
            .expect("catalogに登録されたテーブルはstorageにも必ず存在する");
        executor::seq_scan(mem_table)
    }
    Backend::Disk { storage } => {
        executor::storage_seq_scan(storage, table_info.id, &table_info.schema)?
    }
};
```

`executor::seq_scan(table: &MemTable) -> Vec<Tuple>`は`MemTable`が持つ`Tuple`をそのまま複製するだけでした。
新しく加えた`executor::storage_seq_scan`は、`Storage::scan`(第15章)が返す`(RecordId, バイト列)`から`RecordId`を捨て、バイト列だけを`decode_tuple`(第12章)で`Tuple`へ復元します。

```rust
pub fn storage_seq_scan(storage: &Storage, table_id: TableId, schema: &Schema) -> DbResult<Vec<Tuple>> {
    storage
        .scan(table_id)?
        .map(|entry| entry.and_then(|(_, bytes)| decode_tuple(schema, &bytes)))
        .collect()
}
```

どちらの関数も、戻り値は`DbResult<Vec<Tuple>>`(または`Vec<Tuple>`)という同じ形に揃っています。
`filter`と`project`にとって、この`Vec<Tuple>`がメモリ上の`Vec`から複製されたものか、ページから1件ずつ復元したものかは区別がつきません。
`WHERE`句の三値論理も、`*`の展開も、計算列の型推論も、第10章で書いたコードのまま両方のバックエンドに効いています。

`FROM`を伴わない`SELECT`(`execute_select_without_from`)は、そもそもテーブルを読まないため変更していません。

## UPDATEとDELETEをRecordIdベースにする

`SELECT`と違い、`UPDATE`と`DELETE`は「読んだ行のうちどれを書き換えるか、消すか」を特定しなければなりません。
`MemTable`版の`executor::update`は、この特定を`Vec`の添字で行っています。

```rust
let mut planned = Vec::new();
for (index, tuple) in table.rows().iter().enumerate() {
    // ...(predicateの評価、matchedの判定)
    planned.push((index, Tuple::new(schema, new_values)?));
}

let count = planned.len();
for (index, new_tuple) in planned {
    table.rows_mut()[index] = new_tuple;
}
```

`Storage`にはこの添字に相当するものがありません。
行はページをまたいで散らばっており、1件を指せる唯一の座標は第13章で導入した`RecordId`(ページ番号とスロット番号の組)です。
`storage_update`は、添字の代わりに`Storage::scan`が返す`RecordId`をそのまま使い回します。

```rust
pub fn storage_update(
    storage: &mut Storage,
    table_id: TableId,
    schema: &Schema,
    functions: &FunctionRegistry,
    assignments: &[Assignment],
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    if let Some(pred) = predicate {
        check_predicate_type(pred, schema, functions)?;
    }

    let mut planned: Vec<(RecordId, Tuple)> = Vec::new();
    for entry in storage.scan(table_id)? {
        let (rid, bytes) = entry?;
        let tuple = decode_tuple(schema, &bytes)?;
        // ...(predicateの評価、SETの適用はMemTable版と同じ)
        planned.push((rid, Tuple::new(schema, new_values)?));
    }

    let count = planned.len();
    for (rid, new_tuple) in planned {
        let bytes = encode_tuple(schema, &new_tuple);
        storage.update(table_id, rid, &bytes)?;
    }
    Ok(count)
}
```

書き換える対象を先に全件確定させてから初めて`storage.update`を呼ぶ順序は、`MemTable`版の「1件でも検証に失敗したら`table`は一切変更しない」という不変条件をそのまま引き継いでいます。
`SET`の右辺が更新前の値を見る規則(複数列を書き換える`UPDATE`で、後続の代入が直前の代入結果を見ない)も、`decode_tuple`で復元した`tuple`をもとに評価するので変わりません。
`Storage::update`自身は、新しい値が元のページに収まらない場合に別のページへ移し替え、`RecordId`が変わることがあります(第15章)。
`storage_update`はその移動後の`RecordId`を呼び出し元へ返す必要が無いため(`Database`はその後もう一度`scan`し直すだけです)、この移動をそのまま`Storage::update`に任せています。

`storage_delete`も同じ構造です。
`predicate`に一致した行の`RecordId`を`to_delete`へ集め終えてから、`storage.delete`で1件ずつ削除します。
`MemTable`版の`delete`が生き残る行を集めるのに対し、こちらは消す側を集めるという向きの違いはありますが、「対象を確定させてから書き換える」という順序自体は変わりません。

## Database::openと明示的なflush

永続モードの入口は`Database::open`です。

```rust
pub fn open<P: AsRef<Path>>(path: P) -> DbResult<Self> {
    let path = path.as_ref();
    let storage = if path.exists() {
        Storage::open(path)?
    } else {
        Storage::create(path)?
    };
    Ok(Database {
        functions: FunctionRegistry::with_builtins(),
        backend: Backend::Disk { storage },
    })
}
```

`Storage`は「新規作成」(`create`)と「既存ファイルを開く」(`open`)を別のコンストラクタとして持っていました(第15章)。
`Database::open`は、この2つのどちらを呼ぶべきかを、呼び出し側に選ばせず`path.exists()`だけから決めます。
第1章の冒頭で示した`Database::open("example.db")?`という例は、初回の実行では存在しないパスを渡すので新規作成、2回目以降の実行では存在するパスを渡すので再オープンとして動きます。
利用側のコードは1行のまま、動作だけが状況に応じて変わります。

書き込んだ内容をファイルへ実際に反映させるには、`flush`を呼ぶ必要があります。

```rust
pub fn flush(&self) -> DbResult<()> {
    match &self.backend {
        Backend::Memory { .. } => Ok(()),
        Backend::Disk { storage } => {
            storage.flush()?;
            storage.sync()
        }
    }
}
```

`Storage::flush`は`BufferPool::flush_all`(第14章)をそのまま呼ぶ薄いラッパーで、dirtyなページをOSへ書き渡すところまでしか行いません。
プロセスの再起動をまたいでデータを確実に残すには、そのあとで`DiskManager::sync`(第13章)まで呼ぶ必要があります。
`BufferPool`は`DiskManager`をprivateフィールドとして所有しているため、呼び出し側が`sync`だけを直接呼べる経路はありません。
そこで`BufferPool::sync`と`Storage::sync`という薄いラッパーをそれぞれの層に用意し、`Database::flush`が両方(`flush`→`sync`)を呼ぶことで、呼び出し側からは「`db.flush()`を呼べば耐久化まで完了する」という1つの単純な契約にまとめています。
`flush`と`sync`を呼び分けたい(たとえば複数の変更をまとめて1回だけ`sync`する)場面は当面想定せず、そのための制御は第33章のWALに譲ります。

`Database`に`Drop`を実装して、スコープを抜けるときに自動で`flush`する案も考えられます。
採らなかった理由は、`flush`が`DbResult`を返す(失敗しうる)操作だからです。
`Drop::drop`はエラーを呼び出し元へ返す手段を持たないため、自動flushを実装すると、書き戻しに失敗した場合の選択肢が「無視する」か「panicする」の2つしかなくなります。
`HeapFile::flush`(第13章)も`Storage::flush`(第15章)も、この理由から一貫して呼び出し側に明示的な`flush`を求めており、`Database::flush`もその方針を踏襲しました。
呼び出し側は`db.flush()?`という形で、書き戻しの失敗を他のSQL実行のエラーと同じように扱えます。

## REPLをファイルパスに対応させる

`main.rs`は、起動時の引数からファイルパスを受け取れるようにします。

```rust
let mut args = env::args();
let _program_name = args.next();
let mut db = match args.next() {
    Some(path) => match Database::open(&path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("エラー: {path}を開けませんでした: {e}");
            return;
        }
    },
    None => Database::memory(),
};
```

`cargo run -- example.db`のように引数を渡せば永続モード、渡さなければこれまでどおりインメモリモードで起動します。
終了時は、`\q`の入力でも標準入力のEOFでも同じ後処理を通るように、ループを抜けた直後に`flush`を呼びます。

```rust
if let Err(e) = db.flush() {
    eprintln!("エラー: 終了時のflushに失敗しました: {e}");
}
```

これで、章の冒頭のREPLセッションが次のように変わります。

```console
$ cargo run -- example.db
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> INSERT INTO users VALUES (1, 'Alice');
INSERT 1
minidb> \q
$ cargo run -- example.db
minidb> SELECT * FROM users;
id | name
---------
1 | Alice
(1 row)
```

同じ`example.db`を指定して2回目に起動すると、`users`もその中の`Alice`も、SQLを1文も打ち直すことなく残っています。
テストとしても、このREPLセッションと同じ流れを`Database`の値を作り直す形で確認しています。

```rust
#[test]
fn create_insert_restart_open_select_preserves_the_table_and_its_rows() {
    let path = temp_db_path("restart-basic");
    {
        let mut db = Database::open(&path).unwrap();
        db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)")
            .unwrap();
        db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
            .unwrap();
        db.flush().unwrap();
        // `db`はここでスコープを抜けてdropされる。プロセスの再起動を模している。
    }

    let mut db = Database::open(&path).unwrap();
    let result = db.execute("SELECT id, name FROM users WHERE id = 2").unwrap();
    assert_eq!(result.rows().len(), 1);
    assert_eq!(
        result.rows()[0].values(),
        &[Value::BigInt(2), Value::Text("Bob".to_string())]
    );

    std::fs::remove_file(&path).unwrap();
}
```

`db`が波括弧のブロックを抜けてdropされる箇所が、プロセスの再起動に対応します。
`Storage`はファイルハンドルを閉じる特別な処理を持たないので(`DiskManager`が保持する`std::fs::File`は、Rustの通常の`Drop`でOSにクローズされます)、この`drop`は実際のプロセス終了時に起こることをそのまま再現しています。
同じ`tests/persistence.rs`には、`UPDATE`、`DELETE`が`RecordId`ベースの書き換えや削除を経ても再起動後に結果を保つこと、`DROP TABLE`されたテーブル名が再起動後も再利用できること、そして同じSQLをインメモリモードと永続モードの両方に流して結果が一致することを確認するテストも加えています。

## 第2部の到達点

`CREATE TABLE`、`INSERT`、`SELECT`、`UPDATE`、`DELETE`という5つの文が、`Database::open`を経由するとファイルへ書き込まれ、プロセスを再起動しても`Storage`から復元されるようになりました。
第11章から積み上げてきたFile Header、Slotted Page、Disk Manager、Buffer Pool、永続カタログとFree Space Mapは、この章で初めてSQL文1つひとつの実行経路につながり、`Database::open("example.db")?`という第1章冒頭のコード例が、名前だけでなく中身の伴った約束になりました。

再起動可能なディスクRDBMSが、SQLレベルで動作しています。

第3部では、この`Database`の内部にBinderと名前解決、Logical Plan、Volcano Executorという新しい層を導入し、`SELECT`をより広いSQLサブセットへ広げていきます。
`storage_seq_scan`、`storage_insert`、`storage_update`、`storage_delete`という素朴な関数群は、その最初の実装として当面残りますが、Volcano Executorが導入されれば演算子の抽象化そのものが置き換わります。

## 演習問題

### 必須課題

1. `executor::insert`と`executor::storage_insert`は、どちらも`plan_insert_rows`という共通の関数を呼んでいます。この関数が`table: &mut MemTable`も`storage: &mut Storage`も引数に取らない理由を、`VALUES`の検証と書き込みという2つの仕事のうち`plan_insert_rows`が担うのはどちらか、という観点から説明してください。
2. `Database::catalog()`は`Backend::Disk`に対して呼ぶとpanicします。この設計を、`Option<&Catalog>`を返す設計に変えた場合、`create_drop_create_cycle_succeeds`(第9章由来のテスト)の呼び出し側コードはどう変わるか、実際に書き換えて確認してください。
3. `tests/persistence.rs`の`memory_and_disk_backends_agree_on_the_same_sql`を土台に、`CREATE TABLE`を2回実行して`DbError::DuplicateTable`になる場合も両バックエンドで同じ挙動になることを確認するテストを追加してください。

### 発展課題

1. `storage_update`は、`predicate`に一致した全行を`Vec<(RecordId, Tuple)>`としてメモリ上に集めてから書き戻します。テーブルの行数がバッファプールの容量よりずっと多い場合、この`planned`自体がメモリを圧迫します。`MemTable`版の`update`が同じ設計を採っている理由(第10章)を踏まえて、この設計をこの章でも踏襲した判断が妥当かどうか、行数の想定規模と絡めて考察してください。
2. `Database::open`は`path.exists()`だけを見て`Storage::create`と`Storage::open`のどちらを呼ぶか決めています。`path`が存在するが中身が0バイトの空ファイルだった場合に何が起こるか、実際に試して確認してください。この場合を新規作成として扱うべきか、壊れたファイルとして`DbError`にすべきか、理由とともに考えてください。
3. この章の`Backend`は`enum`で実装しましたが、モジュール冒頭で検討だけした`trait`による抽象化を実際に実装してみてください。`Memory`用と`Disk`用それぞれの実装を用意し、`Database`が`Box<dyn Backend>`(または型引数)を持つ形に書き換えたとき、`execute_*`の各関数のコード量が`enum`版と比べてどう変わるかを比較してください。
