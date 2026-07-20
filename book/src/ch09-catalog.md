# 第9章 カタログとDDL

前章までで、`Database::execute`はSQLの式なら何でも評価できるようになりました。
算術演算、比較演算、三値論理、`IS NULL`、`CAST`、Scalar Function呼び出し、これらを組み合わせた木構造をどれだけ深くしても、`eval_expr`は最後まで値を返します。

```console
minidb> SELECT abs(-5) + CAST('10' AS BIGINT), NULL AND FALSE;
abs(-5) + CAST('10' AS BIGINT) | NULL AND FALSE
-----------------------------------------------
15 | false
(1 row)
```

ところが、次の1行を実行すると`DbError::NotImplemented`が返ります。

```console
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
エラー: 未実装: CREATE TABLEの実行(カタログへの登録)は第9章で対応します
```

構文解析はすでに通っています。
第7章の`Parser`は`CREATE TABLE users (id BIGINT NOT NULL, name TEXT)`を`CreateTableStatement`という正しいASTに組み立てていて、テーブル名も列名も型名も`NOT NULL`の有無も、すべて取り出せる形で手元にあります。
それでも`Database::execute`はそのASTを見なかったことにして、決まり文句のエラーだけを返します。

このSQLサブセットは、`1 = 1`のような式なら評価できるのに、テーブルを1つも作れません。
`SELECT`さえ、`FROM`を付けたとたんに同じ`NotImplemented`に落ちます。
式の評価器がどれだけ精巧でも、そこに流し込む行がどこにも存在しないのでは、データベースとしての体を成していません。

この章で作るのは、テーブルの定義を覚えておく**カタログ**と、それを操作する`CREATE TABLE`と`DROP TABLE`の実行部分です。
テーブルという概念がこの章で初めてこのクレートに生まれます。

## テーブルの定義はどこに置くべきか

`CREATE TABLE users (id BIGINT NOT NULL, name TEXT)`を実行するというのは、具体的には何をすることでしょうか。

最小限には、「`users`という名前と、`id: BIGINT NOT NULL`、`name: TEXT`という列構成の対応」をどこかに記録することです。
第4章で作った`Schema`は、この列構成をすでに正確に表現できる型です。
足りないのは、`Schema`を「`users`という名前」に結び付けて、後から引けるようにする置き場所です。

この置き場所を`Catalog`と呼びます。
`CREATE TABLE`が新しい対応を書き込み、`DROP TABLE`が対応を消し、これから先の章(第10章のDML、第17章のBinder)がテーブル名や列名を解決するたびにここを参照します。
`Catalog`を経由せずにテーブル定義を知る方法をどこにも作らないことで、「`users`というテーブルは今どんな列を持っているか」という問いに対する答えが常に1箇所に定まります。

PostgreSQLの`pg_class`やMySQLの`information_schema`は、テーブルだけでなくインデックス、制約、統計情報まで抱える巨大な対応表です。
この章の`Catalog`はその最小部分、つまりテーブル名と列構成の対応だけを持ちます。
インデックスや制約、統計情報は、それぞれの機能が実装として固まった章で`Catalog`に載せていく計画で、最初から全部を見込んだ大きな構造体を先に設計することはしません。
必要になった機能の分だけ持ち場を広げていく、というこれまでの章と同じ育て方をここでも採ります。
この章では`src/catalog.rs`を新規に作成し、`Catalog`と`TableInfo`をここへ置きます。
`src/lib.rs`には`pub mod catalog;`を追加します。

```rust
pub struct TableInfo {
    pub id: TableId,
    pub name: String,
    pub schema: Schema,
}
```

`name`と`schema`だけで足りそうなところに`id: TableId`を加えているのは、名前とは独立した識別子を持たせるためです。
第3章で導入した`TableId`は、これまで骨格としてだけ存在し、実際に使われる場面がありませんでした。
テーブル名の代わりに`TableId`で参照する場面は、この章にはまだ登場しません。
`INSERT`や`SELECT`が扱う行の集まりを指す場面(第10章)、ディスク上のHeap Fileを指す場面(第13章)になって初めて、名前ではなく数値の識別子で参照する意味が出てきます。
`Catalog`が発行する時点で`TableId`を用意しておけば、それらの章は「テーブルをどう識別するか」という設計判断をあらためて行う必要がありません。

`Catalog`はこの章の時点ではプロセスのメモリ上にしか存在せず、`minidb`を終了すればテーブル定義もろとも消えます。
この定義をディスクに書き出し、再起動後も引き継げるようにするのは第15章の仕事です。

## テーブル名の大文字小文字をどう扱うか

第6章のLexerは、識別子の大文字小文字を畳み込まずに元のテキストのまま`Ident`へ保持する、という決定を下していました。
`users`と`Users`と`USERS`は、字句解析の時点ではまだ「同じ名前かどうか」が決まっていない3つの別々の文字列です。
この判断を先送りしたのは、比較の意味づけを必要とする概念(テーブル)がまだ存在しない章で、比較の規則だけを決めるわけにいかなかったからです。

テーブルという概念は、この章で初めて生まれます。
`CREATE TABLE users (...)`の直後に`CREATE TABLE Users (...)`を実行したとき、これを「同じテーブルの重複作成」として拒否するのか、「別々の2つのテーブル」として両方受け入れるのか、`Catalog`は決めなければなりません。

この章では、**畳み込まない**、つまり大文字小文字を区別する方針を採ります。
`users`と`Users`は別のテーブルとして両方登録でき、`src/catalog.rs`の`create_table`は次の形になります。

```rust
pub fn create_table(&mut self, name: &str, schema: Schema) -> DbResult<TableId> {
    if self.tables.contains_key(name) {
        return Err(DbError::DuplicateTable(name.to_string()));
    }
    // ...
}
```

`HashMap<String, TableInfo>`の`contains_key`は、キーである文字列をバイト単位でそのまま比較します。
特別な畳み込み処理を挟んでいないので、この振る舞いは実装の結果というより、「何もしなければ自然にそうなる」という既定値です。

区別する方針を選んだ理由は、Lexerの決定と揃えるためです。
PostgreSQLは引用符なしの識別子を小文字へ畳み込みますが、その畳み込みはLexerに近い層、あるいは識別子を最初にカタログへ登録する層で行われます。
このクレートのLexerはすでに畳み込まないと決めているので、`Catalog`だけがこっそり大文字小文字を揃えてしまうと、「識別子の大文字小文字はどこで、なぜ変わるのか」という問いに一貫した答えを用意できなくなります。
区別する方針であれば、Lexerから`Catalog`まで、識別子の値は一度も変換されずにそのまま流れます。

この決定には代償もあります。
`users`というテーブルがあるのに`SELECT * FROM Users`と書くと、実在するテーブルが見つからない扱いになります。
多くのSQL実装が引用符なし識別子を大文字小文字を区別せずに解決するのは、この不便さを避けるためです。
このSQLサブセットでは、その不便さより、Lexerからカタログまで識別子が一度も変換されないという単純さを優先しました。

## 守るべき不変条件

実装に入る前に、この章で`Catalog`が保つべき条件を2つ決めます。

1. **同名テーブルの重複登録を許さない**：`CREATE TABLE`が指定した名前のテーブルがすでに存在するなら、新しい`Schema`で古い定義を上書きせず、`DbError::DuplicateTable`を返す
2. **存在しないテーブルの削除を許さない**：`DROP TABLE`が指定した名前のテーブルが存在しないなら、`DbError::TableNotFound`を返す

どちらも一見当たり前に思えますが、これを検査しない`Catalog`は思ったより素直に動いてしまいます。
`HashMap::insert`は同じキーがあれば黙って値を上書きしますし、`HashMap::remove`は無いキーに対しても黙って`None`を返すだけでエラーにはなりません。
「検査しなくても動く」という事実こそが、この2つの条件を明示的なコードとして書く必要がある理由です。
検査を省略しても、`cargo build`はコンパイルを通しますし、`CREATE TABLE users (...)`を2回実行してもクラッシュはしません。
気づかないまま、1回目の`CREATE TABLE`で意図したスキーマがどこにもない、という状況だけが残ります。

この状況は、SQLファイルを2回実行してしまうという、決して珍しくない操作ミスで起こります。
セットアップ用のSQLスクリプトを、すでにテーブルが存在する環境に誤ってもう一度流したとき、`Catalog`が重複を検出してエラーを返してくれれば、そこで実行が止まり、何が起きたかにすぐ気づけます。
検査が無ければ、スクリプトは最後まで「成功」し、後から書き込んだつもりのデータが実は存在しない列を指していた、というような形で不具合が別の場所に飛び火します。
`DROP TABLE`の側も同様で、すでに削除済みのテーブルをもう一度消そうとしたとき、それを「無視して構わない操作」として黙って通すか、それとも「存在するはずのものが無い」という異常として報告するかは、呼び出し側の意図次第です。
この章では後者を選び、`IF EXISTS`のような「無視してよい」という指定を明示的に書く構文は、演習課題として読者に残します。

## `Catalog`の実装

`Catalog`は`src/catalog.rs`にこのように定義し、テーブル名から`TableInfo`を引ける対応表と、次に払い出す`TableId`を持ちます。

```rust
pub struct Catalog {
    tables: HashMap<String, TableInfo>,
    next_table_id: u64,
}
```

`create_table`は`src/catalog.rs`に、前節の重複検査を行ったあと`TableId`を1つ払い出して`tables`に登録する形で書きます。

```rust
pub fn create_table(&mut self, name: &str, schema: Schema) -> DbResult<TableId> {
    if self.tables.contains_key(name) {
        return Err(DbError::DuplicateTable(name.to_string()));
    }

    let id = TableId(self.next_table_id);
    self.next_table_id += 1;
    self.tables.insert(
        name.to_string(),
        TableInfo {
            id,
            name: name.to_string(),
            schema,
        },
    );
    Ok(id)
}
```

`next_table_id`は`create_table`が呼ばれるたびに増えるだけで、`drop_table`で減ることはありません。
`users`を作って`TableId(0)`を受け取り、削除してから同じ名前で作り直すと、次に払い出されるのは`TableId(0)`ではなく`TableId(1)`です。
番号を再利用しない理由は、将来同じ`TableId(0)`が「最初に作った`users`」と「作り直した後の`users`」のどちらを指すか曖昧になる事態を避けるためです。
ディスク上のページやログにテーブルを`TableId`で記録するようになる章(第13章以降)では、この曖昧さがそのままデータの取り違えにつながります。

`drop_table`は`src/catalog.rs`に置き、`HashMap::remove`の戻り値(`Option<TableInfo>`)を、前節で決めた`DbError::TableNotFound`に変換するだけです。

```rust
pub fn drop_table(&mut self, name: &str) -> DbResult<TableId> {
    self.tables
        .remove(name)
        .map(|info| info.id)
        .ok_or_else(|| DbError::TableNotFound(name.to_string()))
}
```

`table`はテーブル名から`TableInfo`を引く読み取り専用の操作で、見つからなければ`None`を返します。
`create_table`や`drop_table`とは違い、「無い」ことがエラーとは限らない場面(たとえば「このテーブルが存在するかどうかを調べたいだけ」)のために、`DbResult`ではなく`Option`を返す形で`src/catalog.rs`に定義します。

```rust
pub fn table(&self, name: &str) -> Option<&TableInfo> {
    self.tables.get(name)
}
```

## `CREATE TABLE`を`Database::execute`に配線する

`src/database.rs`の`Database`に`catalog: Catalog`というフィールドを追加し、`Database::memory()`が空の`Catalog`を用意します。

```rust
pub struct Database {
    functions: FunctionRegistry,
    catalog: Catalog,
}
```

`execute_create_table`は、`CreateTableStatement`の`columns`(`Vec<ColumnDef>`)を`Vec<Column>`に変換してから`Catalog::create_table`を呼びます。
列定義を1件ずつ処理するこの`for`ループでは、型名の解決と合わせて列名の重複も検査する`execute_create_table`を、`src/database.rs`に次のように定義します。

```rust
fn execute_create_table(&mut self, create: &CreateTableStatement) -> DbResult<QueryResult> {
    let mut columns = Vec::with_capacity(create.columns.len());
    let mut seen_names = std::collections::HashSet::with_capacity(create.columns.len());
    for column_def in &create.columns {
        if !seen_names.insert(column_def.name.name.as_str()) {
            return Err(DbError::DuplicateColumn(column_def.name.name.clone()));
        }
        let data_type = DataType::from_sql_name(&column_def.type_name.name).ok_or_else(
            || DbError::Eval(format!("未知の型名です: {}", column_def.type_name.name)),
        )?;
        let nullable = !column_def.not_null;
        columns.push(Column::new(column_def.name.name.clone(), data_type, nullable));
    }

    let schema = Schema::new(columns);
    self.catalog.create_table(&create.table.name, schema)?;
    Ok(QueryResult::command("CREATE TABLE"))
}
```

`seen_names`は列名を1つずつ挿入していく`HashSet<&str>`です。
`HashSet::insert`はすでに同じ値が入っていれば`false`を返すので、`id BIGINT, id TEXT`のように同じ列名が2回現れた時点で`insert`が`false`を返し、`DbError::DuplicateColumn`になります。

```console
minidb> CREATE TABLE dup (id BIGINT, id TEXT);
エラー: 列名が重複しています: id
```

この検査を`Schema::new`自体ではなく`execute_create_table`(`CREATE TABLE`の実行経路)に置いているのは、`Schema`が`CREATE TABLE`の列定義だけでなく、`SELECT`の出力列を表すのにも使われるためです。
`SELECT a, a FROM t`のように、計算結果の列名が重複するのはSQLとして正当なので、`Schema`という型そのものに「列名は必ず一意」という不変条件を持たせることはできません。
一意性が必要なのは実表の列定義という文脈に限られるため、検査はその文脈を知っている`execute_create_table`に置きます。

`nullable`が`!column_def.not_null`の否定になっているのは、`ColumnDef`が`NOT NULL`の有無をそのまま保持しているのに対し、`Column`は「NULLを許すかどうか」という向きでフィールドを持っているためです(第4章)。
`id BIGINT NOT NULL`は`not_null: true`のASTから`nullable: false`の`Column`になり、`name TEXT`(`NOT NULL`無し)は`not_null: false`から`nullable: true`になります。

この`for`ループは、すべての列定義の検査を終えるまで`self.catalog.create_table`を一度も呼びません。
3列目の型名が未知だった場合や、列名が途中で重複していた場合、それより前の列の解決がどれだけ成功していても、`?`または明示的な`return Err(...)`によってループはその場で打ち切られ、`Catalog`には何も登録されません。
「一部だけ登録されたテーブル」という中途半端な状態が生まれないのは、`columns`という`Vec`をローカルに組み立て切ってから、最後に1回だけ`create_table`を呼ぶという順序そのものが保証しています。

型名の解決に使っている`DataType::from_sql_name`は、この章で新しく`src/types.rs`に追加した関数です。

```rust
impl DataType {
    pub fn from_sql_name(name: &str) -> Option<DataType> {
        match name.to_ascii_uppercase().as_str() {
            "BIGINT" => Some(DataType::BigInt),
            "TEXT" => Some(DataType::Text),
            "BOOLEAN" => Some(DataType::Boolean),
            _ => None,
        }
    }
}
```

第8章の`eval`モジュールには、`CAST(expr AS type)`の型名を解決する`resolve_data_type`という、ほぼ同じ内容の関数がすでにありました。
`BIGINT` / `TEXT` / `BOOLEAN`という3つの型名の一覧をこのクレートの2箇所に別々に書いてしまうと、型を1つ追加するたびに両方を直しそびれる不整合の芽になります。
この章では`resolve_data_type`の中身を`DataType::from_sql_name`へ移し、`eval`モジュール側は結果を`DbError::Eval`に包むだけの薄いラッパーに変えました。
`src/eval.rs`の`resolve_data_type`を、次のように書き換えます。

```rust
fn resolve_data_type(type_name: &str) -> DbResult<DataType> {
    DataType::from_sql_name(type_name)
        .ok_or_else(|| DbError::Eval(format!("未知の型名です: {type_name}")))
}
```

`execute_create_table`が未知の型名(`CREATE TABLE t (x FLOAT)`など)に対して同じ`DbError::Eval`を返しているのも、この共有の結果です。
`CAST`の型名エラーと`CREATE TABLE`の型名エラーが、読者から見て同じ理由(型名の一覧に無い名前を渡した)による同じ種類のエラーとして表示されます。

## `DROP TABLE`の構文を追加する

`DROP`と`TABLE`はどちらも第6章のLexerがすでに予約語として持っていましたが、`Parser`側は`DROP`から始まる文をまだ受理しません。
`CreateTableStatement`と対になる`DropTableStatement`を`src/ast.rs`に追加します。

```rust
pub struct DropTableStatement {
    pub table: Ident,
    pub span: Span,
}
```

`parse_drop_table_statement`は`parse_create_table_statement`よりずっと単純です。
`DROP TABLE`に続く列定義の並びが無く、テーブル名を1つ読むだけで終わります。
`src/parser.rs`に追加します。

```rust
fn parse_drop_table_statement(&mut self) -> DbResult<DropTableStatement> {
    let start = self.expect_keyword(Keyword::Drop, "DROP")?.start;
    self.expect_keyword(Keyword::Table, "TABLE")?;
    let table = self.expect_ident()?;
    let end = table.span.end;

    Ok(DropTableStatement {
        table,
        span: Span::new(start, end),
    })
}
```

`parse_statement`の分岐に`TokenKind::Keyword(Keyword::Drop)`を追加すれば、`DROP TABLE users`は`Statement::DropTable`として構文解析を通るようになります。
`src/database.rs`の`execute_drop_table`は、`Catalog::drop_table`をそのまま呼ぶだけです。

```rust
fn execute_drop_table(&mut self, drop: &DropTableStatement) -> DbResult<QueryResult> {
    self.catalog.drop_table(&drop.table.name)?;
    Ok(QueryResult::command("DROP TABLE"))
}
```

`DROP TABLE users`のように削除するテーブル名だけを受け取り、そのテーブルを参照している他のテーブルや索引を巻き込んで消す`CASCADE`、逆に参照があれば拒否する`RESTRICT`のような指定は扱いません。
このSQLサブセットには外部キー制約もインデックスもまだ存在しないので、「他から参照されているかどうか」を考える対象自体がなく、`CASCADE`と`RESTRICT`のどちらを既定にするかという選択も、今の`Catalog`には無関係です。
外部キーが発展編Bで、インデックスが第24章で加わったとき、`drop_table`はそれらの参照を検査する形へ改めて手を入れることになります。

## 完了をどう表現するか

`CREATE TABLE`と`DROP TABLE`は、`SELECT`と違って返す行を持ちません。
これまでの`QueryResult`は`Schema`と`Vec<Tuple>`だけを持つ構造体で、「列が0個で行も0件のSELECT結果」と「DDL文が完了したこと」を区別する手段がありませんでした。
この章では`src/database.rs`の`QueryResult`に`command_tag: Option<&'static str>`というフィールドを追加し、DDL文の完了を表す専用の構築関数を用意します。

```rust
pub struct QueryResult {
    schema: Schema,
    rows: Vec<Tuple>,
    command_tag: Option<&'static str>,
}

impl QueryResult {
    fn command(tag: &'static str) -> Self {
        QueryResult {
            schema: Schema::new(Vec::new()),
            rows: Vec::new(),
            command_tag: Some(tag),
        }
    }
}
```

`src/database.rs`の`Display`実装は`command_tag`が`Some`ならそれだけを表示し、`SELECT`の表形式には進みません。

```rust
impl std::fmt::Display for QueryResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(tag) = self.command_tag {
            return write!(f, "{tag}");
        }
        // ...(以降はSELECTの表形式。前章までと同じ)
    }
}
```

`CREATE TABLE users (...)`を実行すると、REPLには`CREATE TABLE`という1行だけが表示されます。
「影響を受けた行数」を表示する`INSERT`、`UPDATE`、`DELETE`(第10章)とは異なり、DDL文には報告すべき行数という概念自体がありません。
psqlをはじめ多くのクライアントが、DDL文の完了を`CREATE TABLE`や`DROP TABLE`のような文の種類そのものの名前で表示するのは、この章の`command_tag`と同じ考え方です。

## テストで確認する

`catalog`モジュールには、登録、重複拒否、大文字小文字の区別、削除、不存在の拒否、`TableId`が使い回されないことを確認する単体テストを追加しました。
`src/catalog.rs`の`mod tests`に追加します。

```rust
#[test]
fn table_id_is_not_reused_after_drop() {
    let mut catalog = Catalog::new();
    catalog.create_table("users", users_schema()).unwrap();
    catalog.drop_table("users").unwrap();
    let id = catalog.create_table("users", users_schema()).unwrap();
    // 直前に削除した`users`は`TableId(0)`だったが、再作成では`TableId(0)`を
    // 使い回さず、常に単調増加する新しい番号を払い出す。
    assert_eq!(id, TableId(1));
}
```

`database`側には、`CREATE TABLE`がカタログへ正しい`Schema`を登録すること、`NOT NULL`の有無が`nullable`へ正しく反転すること、テーブル名の重複と不存在がそれぞれ`DbError::DuplicateTable`、`DbError::TableNotFound`になること、列名の重複が`DbError::DuplicateColumn`になり、その場合はカタログに何も登録されないことを確認するテストを加えています。
削除してから同じ名前で作り直す一連の流れも、1つのテストにまとめました。
`src/database.rs`の`mod tests`に追加します。

```rust
#[test]
fn create_drop_create_cycle_succeeds() {
    // 削除したテーブル名は再利用できる: 削除→同名で再作成が通ることを確認する。
    let mut db = Database::memory();
    db.execute("CREATE TABLE users (id BIGINT NOT NULL)")
        .unwrap();
    db.execute("DROP TABLE users").unwrap();
    let result = db.execute("CREATE TABLE users (id BIGINT NOT NULL, name TEXT)");
    assert!(result.is_ok());
    assert_eq!(db.catalog().table("users").unwrap().schema.columns().len(), 2);
}
```

Golden Testには、`CREATE TABLE`の完了、存在しないテーブルへの`DROP TABLE`、未知の型名を指定した`CREATE TABLE`の3つを追加しています。
Golden Testの各ファイルは`Database::memory()`ごとに独立した新しいデータベースを1つ作り、SQLを1本だけ実行して結果を突き合わせる仕組みでした(第3章)。
`CREATE TABLE`を2回実行して重複を確認する、あるいは`DROP TABLE`してから作り直すといった、複数のSQL文にまたがる一連の流れはこの仕組みでは表現できません。
そうした流れは、1つの`Database`を使い回せる単体テスト(`create_drop_create_cycle_succeeds`など)の役目として書き分けています。
REPLでも同じ流れを実際に確認できます。

```console
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> CREATE TABLE users (id BIGINT NOT NULL);
エラー: テーブルはすでに存在します: users
minidb> DROP TABLE users;
DROP TABLE
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> DROP TABLE ghost;
エラー: テーブルが存在しません: ghost
```

作成、重複エラー、削除、再作成という一連の流れが、この章のコードだけで最後まで動きます。

```console
$ cargo test
running 120 tests
test catalog::tests::create_table_rejects_duplicate_name ... ok
test catalog::tests::table_id_is_not_reused_after_drop ... ok
test catalog::tests::table_names_are_case_sensitive ... ok
test database::tests::create_table_registers_the_table_in_the_catalog ... ok
test database::tests::create_drop_create_cycle_succeeds ... ok
test database::tests::drop_table_rejects_unknown_table ... ok
test parser::tests::parses_drop_table ... ok
...
test result: ok. 120 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

running 1 test
test golden_tests_pass ... ok
```

## 壊して確認する

`Catalog::create_table`の重複検査を外すと何が起きるか、実際に試します。

```rust
pub fn create_table(&mut self, name: &str, schema: Schema) -> DbResult<TableId> {
    // if self.tables.contains_key(name) { ... } を丸ごとコメントアウト
    let id = TableId(self.next_table_id);
    self.next_table_id += 1;
    self.tables.insert(name.to_string(), TableInfo { id, name: name.to_string(), schema });
    Ok(id)
}
```

この状態で`create_table_rejects_duplicate_name`を実行すると、期待どおり赤くなります。

```console
thread 'catalog::tests::create_table_rejects_duplicate_name' panicked at src/catalog.rs:121:9:
assertion failed: matches!(result, Err(DbError::DuplicateTable(name)) if name == "users")
```

テストが教えてくれるのは「エラーが返らなくなった」という事実だけですが、実際に何が起きているのかはテストの外で確かめる必要があります。
同じ名前で2回`create_table`を呼び、`TableId`と保存された`Schema`がどうなるかを直接調べると、次のようになりました。

```text
first_id=TableId(0) second_id=TableId(1)
stored schema = Schema { columns: [Column { name: "id", data_type: BigInt, nullable: false }, Column { name: "name", data_type: Text, nullable: true }] }
```

1回目の`users`(列は`id`だけ)は`TableId(0)`を受け取りますが、`HashMap::insert`が同じキー`"users"`への2回目の書き込みで古い`TableInfo`を黙って上書きするため、`catalog.table("users")`はもう`TableId(0)`にはたどり着けません。
残るのは2回目に登録した`TableId(1)`と、2回目の`Schema`(`id`と`name`の2列)だけです。
`TableId(0)`という番号自体は、もうどのテーブルも指さない値としてどこにも記録されずに消えます。

重複検査が無い`Catalog`は、クラッシュも警告も出さないまま、最初の`CREATE TABLE`を意味のないものにします。
`users`という名前を再利用したのが別のテーブルを新しく作るつもりだったのか、既存の`users`に列を追加し直すつもりだったのかを、`Catalog`は決して尋ねません。
`contains_key`による1行の検査が無ければ、この区別はコードのどこにも残らないということを、この実験は示しています。

## 演習問題

### 必須課題

1. `Catalog`に、登録されているテーブル名の一覧を返す`table_names(&self) -> Vec<&str>`を追加してください。順序は問いませんが、テストでは順序に依存しない比較(`HashSet`への変換や`sort`など)を使ってください。
2. `DuplicateColumn`の検査は、テーブル名の重複検査と同じく大文字小文字を区別します(`CREATE TABLE t (id BIGINT, ID TEXT)`は別の列名として通ります)。この章で選んだ「テーブル名は大文字小文字を区別する」という方針と、この列名の検査を一貫させる以外に、列名だけ畳み込んで比較する設計にする実益があるかどうかを考察してください。

### 発展課題

1. この章では、テーブル名の大文字小文字を区別する方針を選びました。区別しない方針(`users`と`Users`を同じテーブルとして扱う)に変更するとしたら、`Catalog`のどこを変える必要があるか設計してください。`HashMap<String, TableInfo>`のキーをどう畳み込むか、畳み込んだ後の`TableInfo::name`に元の大文字小文字をどう残すかまで考えてください。
2. `DROP TABLE IF EXISTS users`という構文(テーブルが存在しなくてもエラーにしない)を追加してください。`Lexer`に`IF`と`EXISTS`という新しい予約語が必要かどうか、`DropTableStatement`にフィールドを追加すべきか、`Catalog::drop_table`自体を変更すべきかのどれが適切かを検討したうえで実装してください。
