# 第20章 制約

`users`というテーブルに、同じ`id`を持つ行が2つ入ってしまったら、そのテーブルを使うアプリケーションはどう振る舞うべきでしょうか。

```console
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> INSERT INTO users VALUES (1, 'Alice');
INSERT 1
minidb> INSERT INTO users VALUES (1, 'Alice (別アカウント)');
INSERT 1
```

`id`は`NOT NULL`と宣言してあります。
それでも2回目の`INSERT`はエラーにならず、`users`には`id = 1`の行が2つ並びます。
`SELECT * FROM users WHERE id = 1`は1行を期待して書かれたコードのはずですが、実際には2行返ってきます。
`id`を主キーとして扱うすべてのコード、たとえば`UPDATE users SET name = 'Alicia' WHERE id = 1`のような1行だけを狙った更新も、この章より前の実装では両方の行を書き換えてしまいます。

## 前章の限界

このクレートはすでに`NOT NULL`を持っています。
第10章で`INSERT`と`UPDATE`が`Tuple::new`によるスキーマ検査を通し、`nullable`が`false`の列に`NULL`を入れようとする文を拒否するようになりました。
`id`が`NULL`のまま入ってしまう事故は、この検査によってすでに防げています。

ところが`NOT NULL`が保証するのは「値が空でないこと」だけで、「値が他の行と重なっていないこと」までは見ていません。
`id`が`1`であることと`id`が`NULL`でないことは別の性質であり、後者を検査しても前者の保証にはなりません。
第4章の演習では、この不足を見越して`Key`(Primary Key)という型の設計を発展課題に挙げ、「この型は第9章のカタログとDDLで使うことになります」と書いていました。
実際には第9章の`Catalog`はテーブル名と列構成の対応だけを持つ最小限の実装にとどまり、`Key`に相当する仕組みは実装されないまま、この章まで持ち越されています。

その間、`id`の一意性を守っていたのは何だったのでしょうか。
答えは、DBの外にいる人間、つまりこのクレートを呼び出すアプリケーション側の規律です。
`INSERT`する前に必ず`SELECT ... WHERE id = ?`で存在確認をする、`id`を発行する処理を1箇所に集約して重複が起きないようにする、といった運用ルールを、呼び出し側のコードが自分で守るしかありませんでした。
規律が守られている限りは問題が起きませんが、規律はコードではないので、テストで検出できず、実行時にも警告が出ません。
複数のアプリケーションが同じデータベースへ書き込むようになった瞬間、あるいは既存の存在確認のロジックに1箇所でもバグが混じった瞬間、`id`の重複はDBの外から静かに入り込みます。

この章では、`PRIMARY KEY`と`UNIQUE`という2つの列制約を実装し、一意性の保証をアプリケーション側の規律からDB自身の責務へ移します。
あわせて、複数行にまたがる`INSERT`と`UPDATE`が制約違反で失敗したときに、それより前に処理していた行の変更も一切残らないことを保証する**Statement Rollback**を、これまでの`NOT NULL`だけでなく新しい一意性制約にも広げます。

## `PRIMARY KEY`と`UNIQUE`の構文を追加する

`CREATE TABLE`の列定義に、`PRIMARY KEY`と`UNIQUE`という2つの列制約を追加します。

```console
minidb> CREATE TABLE users (id BIGINT PRIMARY KEY, email TEXT UNIQUE, name TEXT);
CREATE TABLE
```

このSQLサブセットが対応するのは、**単一の列**に対する`PRIMARY KEY`と`UNIQUE`だけです。
複数の列の組み合わせで一意性を課す複合`PRIMARY KEY`(`PRIMARY KEY (a, b)`のような、列定義とは独立したテーブルレベルの制約構文)は、この章では扱いません。
`ColumnDef`という型自体が「1つの列に対する制約」という形をすでに持っているため、単一列の制約はその型にフィールドを足すだけで表現できますが、複合キーは列をまたぐ制約なので、`CreateTableStatement`にテーブルレベルの制約リストを別途持たせる設計変更が要ります。
この設計変更は章末の演習で扱います。

第6章の`src/lexer.rs`にある`Keyword`に、`PRIMARY`、`KEY`、`UNIQUE`という3つの予約語を追加します。

```rust
pub enum Keyword {
    // ...
    Primary,
    Key,
    Unique,
}
```

`src/ast.rs`の`ColumnDef`(第7章のAST)に、`PRIMARY KEY`と`UNIQUE`が指定されていたかどうかを持たせます。

```rust
pub struct ColumnDef {
    pub name: Ident,
    pub type_name: Ident,
    pub not_null: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub span: Span,
}
```

`src/parser.rs`にある`Parser::parse_column_def`は、型名の後ろに`NOT NULL`、`PRIMARY KEY`、`UNIQUE`が任意の順序、任意の個数だけ並ぶ列として読みます。

```rust
fn parse_column_def(&mut self) -> DbResult<ColumnDef> {
    let name = self.expect_ident()?;
    let type_name = self.expect_ident()?;
    let mut end = type_name.span.end;

    let mut not_null = false;
    let mut primary_key = false;
    let mut unique = false;

    loop {
        match self.peek_kind() {
            TokenKind::Keyword(Keyword::Not) => {
                self.advance();
                end = self.expect_keyword(Keyword::Null, "NULL")?.end;
                not_null = true;
            }
            TokenKind::Keyword(Keyword::Primary) => {
                self.advance();
                end = self.expect_keyword(Keyword::Key, "KEY")?.end;
                primary_key = true;
            }
            TokenKind::Keyword(Keyword::Unique) => {
                end = self.advance().span.end;
                unique = true;
            }
            _ => break,
        }
    }

    Ok(ColumnDef {
        span: Span::new(name.span.start, end),
        name,
        type_name,
        not_null,
        primary_key,
        unique,
    })
}
```

`id BIGINT NOT NULL PRIMARY KEY`のように書いても、`id BIGINT PRIMARY KEY NOT NULL`のように書いても、同じ3つのフラグに解決されます。
どちらの順序で書くかは利用者の好みの問題であり、構文としてどちらか一方に決め打つ理由がありません。

`src/types.rs`の`Column`(第4章)にも同じ2つのフラグを追加します。
既存の呼び出し箇所(`Column::new(name, data_type, nullable)`という3引数の呼び出しが、このクレートだけで30箇所以上あります)を1つも壊さないよう、`new`のシグネチャ自体は変えず、これらのフラグはビルダーメソッドで立てる形にします。

```rust
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub primary_key: bool,
    pub unique: bool,
}

impl Column {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Column {
            name: name.into(),
            data_type,
            nullable,
            primary_key: false,
            unique: false,
        }
    }

    pub fn with_primary_key(mut self) -> Self {
        self.primary_key = true;
        self.nullable = false;
        self
    }

    pub fn with_unique(mut self) -> Self {
        self.unique = true;
        self
    }
}
```

`with_primary_key`が`nullable`を`false`へ強制しているのが、`PRIMARY KEY`は`NOT NULL`を含意するというこの章の設計判断そのものです。
`id BIGINT PRIMARY KEY`のように`NOT NULL`を明示しなくても、`PRIMARY KEY`だけで`NULL`を拒否できます。

`src/database.rs`の`Database::execute_create_table`(第9章)は、`ColumnDef`のフラグを`Column`のビルダーメソッドへ橋渡しし、あわせて`PRIMARY KEY`が2列以上に指定されていないかを検査します。

```rust
let mut primary_key_count = 0;
for column_def in &create.columns {
    // ...(列名の重複検査、型名の解決は第9章のまま)
    let mut column = Column::new(column_def.name.name.clone(), data_type, nullable);
    if column_def.primary_key {
        primary_key_count += 1;
        column = column.with_primary_key();
    }
    if column_def.unique {
        column = column.with_unique();
    }
    columns.push(column);
}
if primary_key_count > 1 {
    return Err(DbError::MultiplePrimaryKeys);
}
```

この検査が無いと、`CREATE TABLE t (a BIGINT PRIMARY KEY, b BIGINT PRIMARY KEY)`のような、構文としては書けてしまう複合`PRIMARY KEY`もどきが、意味のはっきりしないまま登録されてしまいます。
`a`と`b`をそれぞれ独立に一意にしたいのか、`(a, b)`の組を一意にしたいのかは、この書き方からは決まりません。
このSQLサブセットは後者(複合キー)にまだ対応していないので、この曖昧さを解決したふりをせず、`DbError::MultiplePrimaryKeys`としてはっきり拒否します。

## 一意性をどう検査するか

`PRIMARY KEY`と`UNIQUE`を宣言しただけでは、まだ何も検査されません。
`Column`にフラグを持たせただけの状態で`INSERT INTO users VALUES (1, 'a@example.com', 'Alice')`に続けて`INSERT INTO users VALUES (1, 'b@example.com', 'Bob')`を実行すると、章の冒頭で見たのと同じ重複がそのまま起きます。
検査を実際に行う場所が必要です。

この章の時点で、テーブルの行を高速に検索できる索引はまだ存在しません。
索引が無い以上、この章の一意性検査は「これから書き込もうとしている値を、テーブルの全行と1つずつ比較する」という線形走査で実装するしかありません。

この走査を、新規作成する`src/constraints.rs`に`check_uniqueness`という関数としてまとめます。

```rust
pub fn check_uniqueness<'a>(
    schema: &Schema,
    others: impl Iterator<Item = &'a Tuple> + Clone,
    candidates: &[Tuple],
) -> DbResult<()> {
    for (index, column) in schema.unique_constrained_columns() {
        for candidate in candidates {
            let value = candidate.get(index).expect("candidateはschemaと同じ列数を持つ");
            if value.is_null() {
                continue;
            }
            if others.clone().any(|other| other.get(index) == Some(value)) {
                return Err(violation_for(column, value));
            }
        }

        for i in 0..candidates.len() {
            let vi = candidates[i].get(index).expect("candidateはschemaと同じ列数を持つ");
            if vi.is_null() {
                continue;
            }
            for candidate in &candidates[i + 1..] {
                let vj = candidate.get(index).expect("candidateはschemaと同じ列数を持つ");
                if !vj.is_null() && vj == vi {
                    return Err(violation_for(column, vi));
                }
            }
        }
    }
    Ok(())
}
```

あわせて`src/lib.rs`に次の1行を加え、このモジュールを公開します。

```rust
pub mod constraints;
```

`schema.unique_constrained_columns()`は、`PRIMARY KEY`または`UNIQUE`が指定された列だけを、Schema上の索引とセットで返す`Schema`の新しいメソッドです。
一意性を検査すべき列が1つも無いテーブル(この章より前に作った、`NOT NULL`だけのテーブルすべてを含みます)では、この関数はループを1度も回さずに`Ok(())`を返します。

`check_uniqueness`は2種類の比較を行います。
1つは`candidates`(これから書き込もうとしている行)を`others`(すでにテーブルにある行、または今回の文で変更されない行)と比べる比較で、既存のデータとの衝突を検出します。
もう1つは`candidates`同士を比べる比較で、`INSERT INTO users VALUES (1, ...), (1, ...)`のように、同じ文の中の複数行が互いに重複している場合を検出します。
`others`だけを見る検査では、このような文内の重複を見逃してしまいます。

`NULL`はどちらの比較からも除外しています(`value.is_null()`のチェック)。
`UNIQUE`列に対して、SQL標準は「`NULL`同士は重複とみなさない」という規則を採ります。
`UNIQUE`な`email`列を持つテーブルに、`email`が`NULL`の行を何行入れても、それらは互いに衝突しません。

```console
minidb> CREATE TABLE users (id BIGINT NOT NULL, email TEXT UNIQUE);
CREATE TABLE
minidb> INSERT INTO users VALUES (1, NULL), (2, NULL);
INSERT 2
```

この規則は恣意的な特例ではなく、「値が分からない」という`NULL`の意味論(第8章で実装した三値論理のUNKNOWN)と整合する取り決めです。
2つの`NULL`が「等しい」とも「等しくない」とも言えない以上、それらを一意性違反として拒否する根拠もありません。
`PRIMARY KEY`列は`with_primary_key`によって`NOT NULL`を含意するため、この規則が実際に効くのは`UNIQUE`列(かつ`NOT NULL`を伴わないもの)に限られます。

`others.clone().any(...)`という部分が、この章のコストの正体です。
`others`は`Clone`を要求されたイテレータで、`candidates`の各行ごとに`others`全体を1回ずつなめ直します。
実際にどれくらいのコストがかかるか、`PRIMARY KEY`を持つテーブルへの`INSERT`を1件だけ測ってみます。
次のコードはクレートのファイルには組み込まず、手元で書いて実行するだけの一時的な測定コードです。

```rust
for &n in &[1_000, 2_000, 4_000, 8_000, 16_000] {
    let mut db = Database::memory();
    db.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    for i in 0..n {
        db.execute(&format!("INSERT INTO t VALUES ({i}, 'x')")).unwrap();
    }
    let start = Instant::now();
    db.execute(&format!("INSERT INTO t VALUES ({n}, 'y')")).unwrap();
    println!("n={n:>6} elapsed={:?}", start.elapsed());
}
```

手元で実行すると、次の結果が得られました。

```text
n=  1000 elapsed=29.25µs
n=  2000 elapsed=49.2µs
n=  4000 elapsed=90.551µs
n=  8000 elapsed=170.431µs
n= 16000 elapsed=349.193µs
```

行数を2倍にするたびに、1件の`INSERT`にかかる時間もほぼ2倍になっています。
`others.clone().any(...)`が線形走査であるという実装の説明を、実測がそのまま裏付けています。
テーブルが数百行程度のうちは気にならない差ですが、行数が伸びるほど、この`INSERT`1件あたりのコストはテーブル全体の行数に比例して伸び続けます。
索引を使わない限り、この比例関係そのものを崩す方法はありません。
第24章で`CREATE INDEX`が使えるようになると、この検査はB+Treeの検索(`O(log n)`)に置き換わり、この比例関係は解消されます。

## Statement Rollback: 制約違反時に文全体を無効にする

`INSERT`と`UPDATE`は、この章より前からAll-or-Nothingの原則を持っていました。
第10章で導入したこの原則は、「複数行の`INSERT`や複数行にまたがる`UPDATE`は、1行でもスキーマ検査に失敗したら、それより前に検査を通っていた行も含めて一切反映しない」というものです。
検査(`Tuple::new`によるスキーマ検査)をすべての対象行に対して済ませてから、最後に1回だけ書き込みを行うという順序そのものが、この原則を実現していました。

この章では、書き込む前に済ませる検査の対象へ、一意性検査(`check_uniqueness`)も加えます。
検査の種類が増えるだけで、「検査をすべて終えるまで書き込みを一切始めない」という順序自体は変わりません。
`src/executor.rs`の`insert`(第10章、インメモリ版)は次のようになります。

```rust
pub fn insert(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    columns: Option<&[usize]>,
    rows: &[Vec<Expr>],
) -> DbResult<usize> {
    let planned = plan_insert_rows(schema, functions, columns, rows)?;
    constraints::check_uniqueness(schema, table.rows().iter(), &planned)?;
    let count = planned.len();
    table.rows_mut().extend(planned);
    Ok(count)
}
```

`plan_insert_rows`が`NOT NULL`と型を検査した`planned`(書き込み候補の`Tuple`列)を組み立て、`check_uniqueness`がその`planned`と既存の全行(`table.rows().iter()`)を比較します。
どちらかの検査が1行でも失敗すれば、`?`によってその場で処理が打ち切られ、`table.rows_mut().extend(planned)`という実際の書き込みには一度も到達しません。

`UPDATE`は`INSERT`より少し込み入っています。
`UPDATE`が変更するのは、テーブルにすでにある行の一部です。
一意性の比較相手を「既存の全行」にそのまま広げると、これから書き換えようとしている行が、書き換わる前の自分自身の値と比較されて、常に衝突してしまいます。
`src/executor.rs`の`update`は、この衝突を避けるため次のように書き換えます。

```rust
let planned_indices: HashSet<usize> = planned.iter().map(|(index, _)| *index).collect();
let candidates: Vec<Tuple> = planned.iter().map(|(_, tuple)| tuple.clone()).collect();
let others = table
    .rows()
    .iter()
    .enumerate()
    .filter(|(index, _)| !planned_indices.contains(index))
    .map(|(_, tuple)| tuple);
constraints::check_uniqueness(schema, others, &candidates)?;
```

`others`は、`WHERE`に一致せず変更されない行だけに絞り込んだイテレータです。
書き換えられる行自身の更新前の値は`others`にも`candidates`にも含めないことで、`UPDATE users SET id = id WHERE id = 1`のような、値を変えない更新まで自分自身との衝突として誤検出することを避けています。
一方で、`candidates`同士の比較(`check_uniqueness`の2つ目のループ)は、`UPDATE users SET email = 'a@example.com' WHERE id <= 2`のように、複数行を同じ値へ書き換えようとした場合の衝突をそのまま検出します。

`Storage`版(`storage_insert`と`storage_update`)も同じ順序を踏みます。
同じ`src/executor.rs`の`storage_update`は、`Storage::scan`で読んだ各行を、`WHERE`に一致するかどうかでその場で`planned`(書き換える行)と`others`(書き換えない行)へ振り分けます。

```rust
let mut planned: Vec<(RecordId, Tuple)> = Vec::new();
let mut others: Vec<Tuple> = Vec::new();
for entry in storage.scan(table_id)? {
    let (rid, bytes) = entry?;
    let tuple = decode_tuple(schema, &bytes)?;
    // ...(predicateの評価、matchedの判定)
    if !matched {
        others.push(tuple);
        continue;
    }
    // ...(assignmentsを適用した新しい値をplannedへpush)
}

let candidates: Vec<Tuple> = planned.iter().map(|(_, tuple)| tuple.clone()).collect();
constraints::check_uniqueness(schema, others.iter(), &candidates)?;

let count = planned.len();
for (rid, new_tuple) in planned {
    let bytes = encode_tuple(schema, &new_tuple);
    storage.update(table_id, rid, &bytes)?;
}
```

`check_uniqueness`を通過して初めて、`storage.update`を呼ぶ`for`ループへ進みます。
このループへ入る前に一意性検査が終わっているため、複数行を書き換える`UPDATE`の後半の行が制約に違反していても、前半の行の`storage.update`は1回も呼ばれません。

ここで保証できる範囲と、保証できない範囲を分けておきます。
`check_uniqueness`を含むすべての検査を通過した**後**の`storage.insert`と`storage.update`自体が、個々の呼び出しで失敗する場合(たとえばページに収まりきらないほど大きい値による`DbError::TupleTooLarge`)は、この章のStatement Rollbackの対象に含まれません。
これは新しく生まれた抜け穴ではなく、第15章がすでに「カタログ永続化失敗の正直なギャップ」として明文化していた割り切りをそのまま引き継いだものです。
`storage.insert`を1件ずつ呼ぶ`for`ループの中で3件目が失敗したとき、それより前に書き込み済みの1件目と2件目を巻き戻す手段は、この章の`Storage`にもまだありません。
この章が実際に閉じたのは、「検査で防げるはずの制約違反」が書き込みの一部を汚してしまう経路であって、「検査を通過した後の物理的な書き込み失敗」が中途半端な状態を残す経路ではありません。
後者を閉じるには、書き込み済みの変更を取り消す仕組み(Undo)が要り、それは第30章のトランザクションと第33章のWrite-Ahead Loggingで手に入ります。

## カタログへの永続化と互換性

`PRIMARY KEY`と`UNIQUE`は`Column`の一部なので、`Storage`(第15章)のCatalogページへも書き出す必要があります。
そうしなければ、`Database::open`で再起動した直後に制約が消えてしまいます。

Catalogページの列ごとのレコードは、これまで次の並びでエンコードされていました。

```text
col_name_len: u16
col_name:     u8 × col_name_len
data_type:    u8 (0=BOOLEAN, 1=BIGINT, 2=TEXT)
nullable:     u8 (0 または 1)
```

この章では、`src/storage.rs`のエンコード処理で`nullable`の直後に`primary_key`と`unique`という2バイトを追加します。

```rust
out.push(data_type_to_u8(column.data_type));
out.push(u8::from(column.nullable));
out.push(u8::from(column.primary_key));
out.push(u8::from(column.unique));
```

同じ`src/storage.rs`の`decode_catalog`側もこの2バイトを読み、`Column`のビルダーメソッドへ渡します。

```rust
let nullable = take_bool(&mut cursor, "nullable")?;
let primary_key = take_bool(&mut cursor, "primary_key")?;
let unique = take_bool(&mut cursor, "unique")?;
let mut column = Column::new(col_name, data_type, nullable);
if primary_key {
    column = column.with_primary_key();
}
if unique {
    column = column.with_unique();
}
```

ここで決めなければならないのが、フォーマット変更の互換方針です。
選択肢は大きく2つあります。
1つは、ページの外枠が持つ`FORMAT_VERSION`(第11章、`Page::decode`がMagic Numberやchecksumと一緒に検証する番号)を上げる方針です。
もう1つは、Catalogページの`payload`のレイアウトだけを変え、`FORMAT_VERSION`には触れない方針です。

この章では後者を選びます。
`FORMAT_VERSION`が保証しているのは、「ページというバイト列の外枠(ヘッダ、checksum、`payload`のサイズ)が読めるか」だけです。
Catalogページの`payload`の中身がどうエンコードされているかは、`page`モジュールの関知するところではなく、`storage`モジュールがこのファイルの中だけで決めている独自のバイナリレイアウトです。
`payload`のレイアウトを変えるたびに`FORMAT_VERSION`を上げる方針を採ると、`payload`の中身に一切関心の無い`page`モジュールが、他のモジュール(このモジュールや、将来増えるページ種別)の内部レイアウト変更のたびに巻き込まれることになります。

代わりに、この教材はそもそも「異なる章のコードでビルドしたデータベースファイル間の互換性」を約束していません。
各章は`git`タグで区切られた1つのスナップショットであり、`Storage::open`が読めるのは、同じ章の`Storage::create`(または、レイアウトを変えていない章)が書いたファイルに限られます。
この章より前のコードで作ったファイルをこの章のコードで開こうとすると、`nullable`の直後で次の列(または`page_count`)を読もうとして境界がずれ、`DbError::CorruptCatalog`になります。
これは新しい方針ではなく、第15章でこのモジュールが生まれたときから前提にしていたことを、実際に`payload`のレイアウトが変わったこの章で初めて明文化したものです。

## テストで確認する

`src/constraints.rs`には、既存行との重複検出、文内の重複検出、`NULL`同士が衝突しないこと、制約を持たないテーブルでは何も検査しないことを確認する単体テストを追加します。

```rust
#[test]
fn detects_duplicate_among_candidates_themselves() {
    let schema = schema_with_unique_email();
    let candidates = vec![tuple(&schema, 1, Some("a@example.com")), tuple(&schema, 2, Some("a@example.com"))];
    let err = check_uniqueness(&schema, std::iter::empty(), &candidates).unwrap_err();
    assert!(matches!(err, DbError::UniqueViolation { column, .. } if column == "email"));
}
```

`src/database.rs`には、`PRIMARY KEY`と`UNIQUE`の挿入時と更新時の違反、文内重複、`NULL`と`UNIQUE`の共存に加えて、Statement Rollbackを直接狙ったテストを追加しています。

```rust
#[test]
fn update_statement_rollback_leaves_earlier_rows_untouched_on_later_violation() {
    let mut db = users_with_constraints_db();
    db.execute(
        "INSERT INTO users VALUES (1, 'a@example.com', 'Alice'), (2, 'b@example.com', 'Bob'), (3, 'c@example.com', 'Carol')",
    )
    .unwrap();
    // id=1とid=2をどちらも'a@example.com'へ書き換えようとする更新。
    // 2行目を処理した時点で1行目との重複が判明し、文全体が失敗する。
    let result = db.execute("UPDATE users SET email = 'a@example.com' WHERE id <= 2");
    assert!(matches!(result, Err(DbError::UniqueViolation { .. })));

    let alice = db.execute("SELECT email FROM users WHERE id = 1").unwrap();
    assert_eq!(alice.rows()[0].values(), &[Value::Text("a@example.com".to_string())]);
    let bob = db.execute("SELECT email FROM users WHERE id = 2").unwrap();
    assert_eq!(bob.rows()[0].values(), &[Value::Text("b@example.com".to_string())]);
}
```

`id = 1`の行は、`UPDATE`の対象(`WHERE id <= 2`)に含まれていたにもかかわらず、`email`は更新前の`'a@example.com'`のままです。
`id = 2`の行との重複が判明した時点で文全体が打ち切られ、`id = 1`の行に対する変更もテーブルへは一切反映されていません。

`tests/persistence.rs`には、`PRIMARY KEY`と`UNIQUE`を持つテーブルを永続モードで作り、`flush`してから再オープンしても制約が引き続き効くことを確認するテストを、`tests/differential.rs`には、SQLiteとの比較テストを追加します。
SQLiteとの比較では、両者のエラーメッセージの文言までは一致させません。
minidbは`PRIMARY KEY制約違反です: 列'id'の値1が重複しています`、SQLiteは`UNIQUE constraint failed: users.id`のように、エラーメッセージの語彙や形式はもともと独立に決められたもので、文字列としての一致を求める意味がありません。
比較するのは「制約違反の文がエラーとして拒否されること」という意味論の一致だけです。
`tests/differential.rs`には、次の`assert_both_error`を用意します。

```rust
fn assert_both_error(setup: &[&str], failing_statement: &str) {
    let mut minidb = Database::memory();
    for statement in setup {
        minidb.execute(statement).unwrap();
    }
    assert!(minidb.execute(failing_statement).is_err());

    let conn = Connection::open_in_memory().unwrap();
    for statement in setup {
        conn.execute(statement, []).unwrap();
    }
    assert!(conn.execute(failing_statement, []).is_err());
}
```

この比較には、もう1つ注意が要ります。
SQLiteは、列の型名が正確に`INTEGER`である場合に限り、その`PRIMARY KEY`列をrowidの別名として扱い、`NULL`を「次のrowidを自動採番する」特別な値として受理します。
さらにSQLiteでは、`PRIMARY KEY`単体は標準SQLとは異なり`NOT NULL`を含意しません(rowidの別名にならない`PRIMARY KEY`列には、明示的な`NOT NULL`が無い限り`NULL`が入ります)。
このSQLサブセットの`BIGINT PRIMARY KEY`は型名が`INTEGER`ではないためrowidの別名にはなりませんが、「`PRIMARY KEY`は`NOT NULL`を含意しない」というSQLite側の緩さは、型名に関係なく残ります。
`PRIMARY KEY`列への`NULL`挿入を両エンジンで比較するテストでは、この差異を踏まないよう、`NOT NULL`を明示した列(`id BIGINT PRIMARY KEY NOT NULL`)を使っています。

```console
$ cargo test
test result: ok. 378 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

## 壊して確認する

`constraints::check_uniqueness`から、`candidates`同士を比較する2つ目のループだけを外すと何が起きるか、次のように試しに崩してみます。

```rust
// for i in 0..candidates.len() { ... } のブロックを丸ごとコメントアウト
```

この状態で`insert_rejects_duplicate_within_the_same_statement`(同じ`INSERT`文の中の2行が重複しているテスト)を実行すると、期待どおり赤くなります。

```console
thread 'database::tests::insert_rejects_duplicate_within_the_same_statement' panicked at src/database.rs:1390:9:
assertion failed: matches!(result, Err(DbError::UniqueViolation { column, .. }) if column == "email")
```

`others`との比較(1つ目のループ)は健在なので、既存の行との重複は変わらず検出されます。
検出できなくなるのは、`INSERT INTO users VALUES (1, 'a@example.com', 'Alice'), (2, 'a@example.com', 'Bob')`のように、1つの文の中だけで初めて重複が生じるケースに限られます。
このINSERTを実行すると、`others`にはまだ`email = 'a@example.com'`の行が無いので1つ目のループを素通りし、`candidates`同士の比較が無いので2つ目のループでも何も検出されず、2行とも挿入に成功してしまいます。

```console
minidb> INSERT INTO users VALUES (1, 'a@example.com', 'Alice'), (2, 'a@example.com', 'Bob');
INSERT 2
minidb> SELECT id, email FROM users;
id | email
----------
1 | a@example.com
2 | a@example.com
(2 rows)
```

`UNIQUE`と宣言した列に、同じ値を持つ行が2つ並んでいます。
`others`との比較だけでは「今までのテーブルの状態」しか見ておらず、「今まさに書き込もうとしている行同士の関係」を見落とすことを、この実験は示しています。
1行ずつの`INSERT`を2回に分けて実行していれば、1回目の`INSERT`の後にはすでに`others`の中に`'a@example.com'`が存在するため、2回目の`INSERT`は正しく拒否されます。
複数行をまとめた1本の文だからこそ、この抜け穴が意味を持つわけです。

## 演習問題

### 必須課題

1. `UNIQUE`を複数の列に指定したテーブル(`CREATE TABLE t (a BIGINT UNIQUE, b BIGINT UNIQUE)`)に対して、`a`だけが重複する行と`b`だけが重複する行をそれぞれ`INSERT`し、意図した列のエラー(`DbError::UniqueViolation`の`column`フィールド)になることを確認するテストを追加してください。
2. `DELETE`は`PRIMARY KEY`と`UNIQUE`の検査対象になりません。行を減らす操作がなぜ一意性検査を必要としないのか、`check_uniqueness`のシグネチャ(`others`と`candidates`の役割)に触れながら説明してください。

### 発展課題

1. 複合`PRIMARY KEY`(`CREATE TABLE t (a BIGINT, b BIGINT, PRIMARY KEY (a, b))`のような、列定義とは独立したテーブルレベルの制約構文)を設計してください。`CreateTableStatement`にどんなフィールドを追加すべきか、`Schema`または`Column`のどちらに「この列の組は複合キーの一部である」という情報を持たせるべきかを検討したうえで、`check_uniqueness`を複数列の組に対応させる変更まで実装してください。
2. `check_uniqueness`は、`candidates`の行数を`m`、`others`の行数を`n`とすると、最悪`O(n × m)`の比較を行います。第24章より前にこのコストを下げる方法として、`INSERT`の実行中だけ`candidates`の値を`HashSet`(または`BTreeSet`)へ積んでおき、文内の重複検出だけを`O(m)`にする案が考えられます。この案を実装し、`others`との比較(`O(n × m)`のまま残る部分)と合わせてどれだけ改善するかを、本章の測定と同じ方法で測ってください。
