# 第10章 インメモリ表とDML

前章までで、`CREATE TABLE users (id BIGINT NOT NULL, name TEXT)`は`Catalog`にテーブル定義を登録するところまで動くようになりました。

```console
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
```

ところが、このテーブルに1行入れようとすると、次のようにエラーが返ります。

```console
minidb> INSERT INTO users VALUES (1, 'Alice');
エラー: 未実装: INSERTの実行(表への追加)は第10章で対応します
minidb> SELECT id FROM users;
エラー: 未実装: FROM・WHEREを伴うSELECTの実行(表の中身を読む手段)は第10章で対応します
```

`users`というテーブルは存在します。
列の名前も型も`NOT NULL`の有無も、`Catalog`が正確に覚えています。
それでも、`users`に対応する行の集まりがどこにも無いので、1行も入れられず、1行も読み出せません。
テーブルは作れるのに使えない、というのが前章の終わり時点の状態です。

この章では、テーブルの行そのものを保持する場所を作り、`INSERT`、`SELECT ... FROM`、`UPDATE`、`DELETE`を実行できるようにします。
これで、テーブルの作成からデータの出し入れまでが1つのクレートの中だけで完結する、インメモリのSQLデータベースが完成します。

## テーブルの中身をどこに置くか

`Catalog`(第9章)が持っているのは、`users`という名前と`id BIGINT NOT NULL`、`name TEXT`という列構成の対応だけです。
行の集まりはどこにも紐付いていません。

この行の集まりを、`Catalog`に直接持たせる案も考えられます。
しかし、`Catalog`はテーブルの「定義」を扱う場所として第9章で設計しました。
`TableInfo`に`rows: Vec<Tuple>`のようなフィールドを足すと、「名前から定義を引く場所」と「名前から中身を引く場所」が同じ構造体に同居することになり、ディスクへの永続化が入る第2部で、定義と中身をそれぞれ別のファイル、別のページに分けて書き出したくなったときに、両者が絡み合ったままのコードを解きほぐす作業から始めることになります。

そこで、行の集まりは`storage_mem`という新しいモジュールに置きます。

```rust
pub struct MemTable {
    rows: Vec<Tuple>,
}

pub struct MemStorage {
    tables: HashMap<TableId, MemTable>,
}
```

`MemStorage`は`TableId`をキーにする対応表です。
テーブル名ではなく`TableId`をキーに選んだのは、`Catalog`がテーブル名から`TableId`をすでに引けるからです。
`Database`は、SQL文に出てくる`users`という名前を`Catalog`で1回だけ`TableId`に変換すれば、以降は`Catalog`と`MemStorage`のどちらも同じ`TableId`で参照できます。
`CREATE TABLE`は`Catalog::create_table`が払い出した`TableId`で`MemStorage::create_table`を呼び、`DROP TABLE`も同じ`TableId`で両方から取り除きます。
「`Catalog`にはあるのに`MemStorage`には無いテーブル」という状態が生まれるとしたら、それはこの2箇所の呼び出しをどちらか片方だけ書き忘れたときだけです。

## 列参照をどう解決するか

`WHERE id = 1`や`SET name = 'Bob'`を実行するには、式の中に出てくる`id`や`name`が指す値を、今処理している行から引けなければなりません。
第8章の`eval_expr`は、`Expr::ColumnRef`に出会うと`DbError::NotImplemented`を返していました。
値を名前から引く環境がまだ存在しなかったからです。

その環境を`Row`という型で導入します。

```rust
pub struct Row<'a> {
    schema: &'a Schema,
    tuple: &'a Tuple,
}

impl<'a> Row<'a> {
    pub fn new(schema: &'a Schema, tuple: &'a Tuple) -> Self {
        Row { schema, tuple }
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.tuple.get_by_name(self.schema, name)
    }
}
```

`Row`は`Schema`と`Tuple`を1組にまとめただけの薄いラッパーです。
列名から値を引く`get`は、第4章で作った`Tuple::get_by_name`をそのまま呼ぶだけで実装できます。
`Schema`と`Tuple`をペアで持ち回らずに済むよう1つの型にまとめておくと、`eval_expr`の引数が1個で済み、呼び出し側も「今どの行を処理しているか」を`Row`という1つの値で受け渡せます。

`eval_expr`のシグネチャに、この`Row`を`Option`で受け取る引数を追加します。

```rust
pub fn eval_expr(expr: &Expr, functions: &FunctionRegistry, row: Option<&Row>) -> DbResult<Value> {
    match expr {
        // ...
        Expr::ColumnRef { name, .. } => match row {
            Some(row) => row
                .get(name)
                .cloned()
                .ok_or_else(|| DbError::Eval(format!("列'{name}'が見つかりません"))),
            None => Err(DbError::Eval(format!(
                "列参照'{name}'は行を伴わない文脈では使えません"
            ))),
        },
        // ...
    }
}
```

`Option`にしたのは、行を伴わない文脈が実際に存在するからです。
`INSERT INTO users VALUES (1, 'Alice')`の`VALUES`は、まだどの行にも属していない値を組み立てているだけで、参照できる既存の行がありません。
`row`が`None`のときに列参照が現れたら、それは構文として書けてしまっただけの誤った式なので、`DbError::Eval`にします。
`row`が`Some`でも、`Row`が持つ`Schema`に無い列名を指していれば、同じく`DbError::Eval`です。
`users`に無い`age`という列を`WHERE age = 1`のように書いた場合がこれに当たります。

この変更は、`eval_expr`を呼んでいたすべての箇所に影響します。
第8章までのテストやSELECTの実装は、行を必要としない式しか扱っていなかったので、呼び出し側は`row`に`None`を渡すだけで動き続けます。

## 実行演算子をどう区切るか

`INSERT`、`SELECT`、`UPDATE`、`DELETE`をそれぞれ`Database::execute`の1つのメソッドにベタ書きすることもできます。
しかし、`SELECT`の`WHERE`が三値論理で行を絞り込む処理も、`DELETE`の`WHERE`が行を絞り込む処理も、中身は同じです。
両方に同じロジックを別々に書けば、三値論理の扱いを直すときに2箇所を揃えて直し忘れる余地が生まれます。

そこで、SQL文の実行を、関係代数の演算子に対応する小さな関数へ分解します。

* **Values**: `VALUES`の各行を評価し、`Value`の並びにする
* **Sequential Scan**: テーブルの全行を、格納順のまま読み出す
* **Filter**: `WHERE`の述語を評価し、`TRUE`になった行だけを残す
* **Projection**: `SELECT`の対象式リストを適用し、出力用の列と行を組み立てる
* **Insert**: 評価済みの行を`MemStorage`へ書き込む
* **Update**: `Filter`で絞った行に`SET`を適用する
* **Delete**: `Filter`で絞った行を取り除く

`Executor::next()`が1行ずつ値を引っ張り出す、いわゆるVolcanoモデルの実行エンジンは第19章で作ります。
この章の演算子は、テーブル全体を`Vec<Tuple>`としてまとめて受け取り、まとめて返す素朴な関数にとどめます。
1行ずつ処理するかテーブルごとまとめて処理するかは、演算子が何を計算するかとは独立した実装上の選択です。
先に正しい計算内容を固めておけば、第19章で1行ずつのパイプラインへ組み替えるときも、各演算子が「何を返すべきか」は変わりません。

これらの関数はすべて`executor`という新しいモジュールに置きます。
`Database::execute`側の各メソッドは、`Catalog`でテーブル定義を引き、`MemStorage`から`MemTable`を取り出し、`executor`の関数を正しい順序で呼ぶだけの配線役になります。

## 守るべき不変条件

実装に入る前に、この章のDMLが守るべき条件を3つ決めます。

1. **WHEREはTRUEの行だけを採用する**: SQLの`WHERE`は三値論理で評価するため、`FALSE`になった行はもちろん、`UNKNOWN`(`NULL`)になった行も結果から落とす
2. **INSERT、UPDATEはAll-or-Nothing**: 複数行の`INSERT`や、複数行にまたがる`UPDATE`は、1行でもスキーマ検査(`NOT NULL`違反など)に失敗したら、それより前に検査を通っていた行も含めて一切反映しない
3. **UPDATEのSET右辺は更新前の行を見る**: `SET a = b, b = a`のように複数列を書き換えるとき、後続の代入は直前の代入結果ではなく、その行の更新前の値を使う

1番目は、第8章で実装した三値論理をそのまま`Filter`演算子に持ち込むだけで満たせます。
2番目と3番目は、`CREATE TABLE`が列定義をすべて検査してから最後に1回だけ`Catalog`へ登録する(第9章)のと同じ考え方です。
「一部だけ書き込まれた`INSERT`」や「一部の行だけ書き換わった`UPDATE`」という中途半端な状態を、検査より先に書き込みを始めないという順序そのもので防ぎます。

## `INSERT`を実装する

第7章の`InsertStatement`は、`INSERT INTO name VALUES (...)`という1行分の挿入にしか対応していませんでした。
この章では、複数行の`VALUES (...), (...)`と、列名を明示する`INSERT INTO name (col, ...)`の両方に対応させます。

```rust
pub struct InsertStatement {
    pub table: Ident,
    pub columns: Option<Vec<Ident>>,
    pub rows: Vec<Vec<Expr>>,
    pub span: Span,
}
```

`Parser`側は、`(`が続けば列名の並びを読み、`VALUES`に続く行を`,`区切りで読めるだけ読みます。

```rust
let columns = if *self.peek_kind() == TokenKind::LParen {
    self.advance();
    let mut columns = vec![self.expect_ident()?];
    while *self.peek_kind() == TokenKind::Comma {
        self.advance();
        columns.push(self.expect_ident()?);
    }
    self.expect_punct(TokenKind::RParen, ")")?;
    Some(columns)
} else {
    None
};

self.expect_keyword(Keyword::Values, "VALUES")?;

let (first_row, mut end) = self.parse_values_row()?;
let mut rows = vec![first_row];
while *self.peek_kind() == TokenKind::Comma {
    self.advance();
    let (row, row_end) = self.parse_values_row()?;
    rows.push(row);
    end = row_end;
}
```

実行側の`executor::insert`は、Values演算子(各行の式を評価する部分)とInsert演算子(評価済みの行を書き込む部分)を1つの関数にまとめています。

```rust
pub fn insert(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    columns: Option<&[Ident]>,
    rows: &[Vec<Expr>],
) -> DbResult<usize> {
    let mut planned = Vec::with_capacity(rows.len());
    for row_exprs in rows {
        let evaluated = row_exprs
            .iter()
            .map(|expr| eval_expr(expr, functions, None))
            .collect::<DbResult<Vec<_>>>()?;
        let full_values = expand_to_schema(schema, columns, evaluated)?;
        planned.push(Tuple::new(schema, full_values)?);
    }

    let count = planned.len();
    table.rows_mut().extend(planned);
    Ok(count)
}
```

`VALUES`の各要素を評価するとき、`eval_expr`に渡す`Row`は常に`None`です。
挿入しようとしている値が、既存の行を参照する構文はこのSQLサブセットに無いため、行環境を用意する必要がありません。

`planned`という`Vec<Tuple>`にすべての行の`Tuple::new`が成功してから、最後に`table.rows_mut().extend(planned)`で1回だけ書き込んでいるのは、「守るべき不変条件」の2番目(All-or-Nothing)を満たすためです。
3行目の型が`NOT NULL`列に違反していれば、`?`によってその場でこの関数全体が打ち切られ、`table`は一切変更されません。

列名指定がある場合の並べ替えは、`expand_to_schema`が担当します。

```rust
fn expand_to_schema(
    schema: &Schema,
    columns: Option<&[Ident]>,
    values: Vec<Value>,
) -> DbResult<Vec<Value>> {
    let Some(columns) = columns else {
        return Ok(values);
    };

    if columns.len() != values.len() {
        return Err(DbError::Eval(format!(
            "列の個数({})とVALUESの個数({})が一致しません",
            columns.len(),
            values.len()
        )));
    }

    let mut full = vec![Value::Null; schema.len()];
    let mut assigned = vec![false; schema.len()];
    for (column, value) in columns.iter().zip(values) {
        let index = schema
            .index_of(&column.name)
            .ok_or_else(|| DbError::Eval(format!("列'{}'が見つかりません", column.name)))?;
        if assigned[index] {
            return Err(DbError::Eval(format!(
                "列'{}'がINSERTの列リストに重複しています",
                column.name
            )));
        }
        assigned[index] = true;
        full[index] = value;
    }
    Ok(full)
}
```

`INSERT INTO users (id) VALUES (1)`のように、`name`列を指定しなかった場合、`full`はあらかじめ`Value::Null`で埋めた`Vec`として組み立てます。
`name`が`NOT NULL`列であれば、この`Value::Null`は後段の`Tuple::new`によるスキーマ検査ではじかれます。
列名指定を省略した場合(`columns`が`None`)は、`VALUES`の並びをそのままスキーマの列順とみなし、個数の食い違いは`Tuple::new`の列数検査に任せます。

## `SELECT`の`FROM`、`WHERE`、`*`を実装する

`SELECT`の対象式リストに`*`を書けるようにするには、まずASTを拡張する必要があります。
`*`はLexerの上では乗算のToken(`TokenKind::Star`)と同じもので、これまでの`SelectItem`は式1個を持つ構造体でした。
`*`を式として扱うのではなく、`SelectItem`自体を列挙型にします。

```rust
pub enum SelectItem {
    Expr { expr: Expr, span: Span },
    Wildcard { span: Span },
}
```

`Parser`は、対象式リストの要素を読む直前に`*`かどうかを先読みして振り分けます。

```rust
fn parse_select_item(&mut self) -> DbResult<SelectItem> {
    if *self.peek_kind() == TokenKind::Star {
        let span = self.advance().span;
        return Ok(SelectItem::Wildcard { span });
    }
    let expr = self.parse_expr(0)?;
    let span = expr.span();
    Ok(SelectItem::Expr { expr, span })
}
```

実行側は、`FROM`が無ければ第9章までと同じ経路(その場で式を評価し、1行だけ返す)を使い、`FROM`があればSequential Scan、Filter、Projectionの3演算子を順に適用します。

```rust
let table_info = self
    .catalog
    .table(table_name)
    .ok_or_else(|| DbError::TableNotFound(table_name.to_string()))?;
let mem_table = self
    .storage
    .table(table_info.id)
    .expect("catalogに登録されたテーブルはstorageにも必ず存在する");

let scanned = executor::seq_scan(mem_table);
let filtered = match &select.where_clause {
    Some(predicate) => {
        executor::filter(&table_info.schema, &self.functions, scanned, predicate)?
    }
    None => scanned,
};
let (schema, rows) = executor::project(
    &table_info.schema,
    &self.functions,
    &filtered,
    &select.items,
    sql,
)?;
```

Filter演算子は、`守るべき不変条件`の1番目をそのままコードにしただけです。

```rust
pub fn filter(
    schema: &Schema,
    functions: &FunctionRegistry,
    rows: Vec<Tuple>,
    predicate: &Expr,
) -> DbResult<Vec<Tuple>> {
    let mut kept = Vec::with_capacity(rows.len());
    for tuple in rows {
        let row = Row::new(schema, &tuple);
        if matches!(
            eval_expr(predicate, functions, Some(&row))?,
            Value::Boolean(true)
        ) {
            kept.push(tuple);
        }
    }
    Ok(kept)
}
```

`eval_expr`が返す`Value`が`Boolean(true)`かどうかだけを見ているので、`Boolean(false)`はもちろん、`Value::Null`(三値論理のUNKNOWN)もこの`if`を通らず、`kept`に積まれません。
「UNKNOWNの行を、一致しなかった行と同じように扱う」という三値論理の規則は、`eval`モジュール(第8章)が計算した結果をそのまま使うだけで、この関数自身が特別な分岐を書く必要はありませんでした。

Projection演算子は、`*`をテーブルの全列参照へ展開してから、「列参照または式のリスト」という1種類の形だけを扱います。

```rust
fn resolve_items(table_schema: &Schema, items: &[SelectItem], sql: &str) -> Vec<(Expr, String)> {
    let mut resolved = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard { span } => {
                for column in table_schema.columns() {
                    resolved.push((
                        Expr::ColumnRef {
                            name: column.name.clone(),
                            span: *span,
                        },
                        column.name.clone(),
                    ));
                }
            }
            SelectItem::Expr { expr, span } => {
                resolved.push((expr.clone(), sql[span.start..span.end].to_string()));
            }
        }
    }
    resolved
}
```

出力列の型と`nullable`は、単純な列参照であれば`table_schema`の定義をそのまま使います。
これなら、行が1件も無いテーブルに対する`SELECT id FROM empty_table`でも、列の型を正確に決められます。
一方、`id + 1`のような計算結果は、まだ静的な型検査を持たない(それは第17章のBinderの仕事です)ため、実際に1行評価してみて、その結果から型を決めるという折衷案を採ります。

```rust
let data_type = match rows.first() {
    Some(first) => {
        let row = Row::new(table_schema, first);
        eval_expr(expr, functions, Some(&row))?
            .data_type()
            .unwrap_or(DataType::Text)
    }
    None => DataType::Text,
};
out_columns.push(Column::new(name.clone(), data_type, true));
```

行が1件も無ければ`TEXT`で代用します。
これは、第8章で`SELECT NULL`の結果列に`TEXT`を仮の型として使ったのと同じ折衷です。
計算結果の`nullable`を常に`true`にしているのは、行ごとに`NULL`になったりならなかったりしうる式の`nullable`を、1行だけ覗いて`false`と決め打ってしまうと、2行目以降で本当に`NULL`が出てきたときに`Tuple::new`のスキーマ検査がその`NULL`を不当に拒否してしまうからです。
列参照そのものであれば`table_schema`の`nullable`をそのまま使えるので、この問題は起きません。

## `UPDATE`と`DELETE`を実装する

`UPDATE users SET name = 'Bob' WHERE id = 1`のASTは、`SET`のカンマ区切りリストを`Assignment`の並びとして持ちます。

```rust
pub struct UpdateStatement {
    pub table: Ident,
    pub assignments: Vec<Assignment>,
    pub where_clause: Option<Expr>,
    pub span: Span,
}

pub struct Assignment {
    pub column: Ident,
    pub value: Expr,
    pub span: Span,
}
```

Update演算子は、`WHERE`に一致した行(`predicate`が無ければ全行)それぞれに`assignments`を適用します。

```rust
pub fn update(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    assignments: &[Assignment],
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    let mut planned = Vec::new();
    for (index, tuple) in table.rows().iter().enumerate() {
        let row = Row::new(schema, tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => matches!(
                eval_expr(pred, functions, Some(&row))?,
                Value::Boolean(true)
            ),
        };
        if !matched {
            continue;
        }

        let mut new_values = tuple.values().to_vec();
        for assignment in assignments {
            let target = schema.index_of(&assignment.column.name).ok_or_else(|| {
                DbError::Eval(format!("列'{}'が見つかりません", assignment.column.name))
            })?;
            new_values[target] = eval_expr(&assignment.value, functions, Some(&row))?;
        }
        planned.push((index, Tuple::new(schema, new_values)?));
    }

    let count = planned.len();
    for (index, new_tuple) in planned {
        table.rows_mut()[index] = new_tuple;
    }
    Ok(count)
}
```

`SET`の右辺を評価するとき、`assignment`ごとに`eval_expr(&assignment.value, functions, Some(&row))`を呼んでいますが、この`row`は`for`ループの先頭で作った、更新前の`tuple`を指したままの`Row`です。
`new_values`は代入結果を書き込む先であって、評価に使う`row`とは別の変数なので、`SET id = id + 1, name = name`のように複数列を同時に書き換えても、後続の代入が直前の代入結果を見ることはありません。
これが「守るべき不変条件」の3番目です。

INSERT演算子と同じく、書き換え後の`Tuple`をすべて`planned`に集め終えてから、最後に一括で`table`へ反映しています。
`assignments`の適用中に型検査(`Tuple::new`)が1行でも失敗すれば、それより前に検査を通っていた行も含めて`table`は変更されません。

Delete演算子は、`Filter`演算子と対になる形をしています。

```rust
pub fn delete(
    table: &mut MemTable,
    schema: &Schema,
    functions: &FunctionRegistry,
    predicate: Option<&Expr>,
) -> DbResult<usize> {
    let mut kept = Vec::with_capacity(table.rows().len());
    let mut deleted = 0usize;
    for tuple in table.rows() {
        let row = Row::new(schema, tuple);
        let matched = match predicate {
            None => true,
            Some(pred) => matches!(
                eval_expr(pred, functions, Some(&row))?,
                Value::Boolean(true)
            ),
        };
        if matched {
            deleted += 1;
        } else {
            kept.push(tuple.clone());
        }
    }

    *table.rows_mut() = kept;
    Ok(deleted)
}
```

`Filter`が「`predicate`がTRUEの行を残す」のに対し、`delete`は「`predicate`がTRUEの行を取り除く」ので、生き残る行(`kept`)は`Filter`とは逆の条件で集めます。
`kept`を組み立て終える前に`eval_expr`が失敗すれば、その時点で`table`はまだ元のままなので、失敗した`DELETE`が一部の行だけ消してしまうことはありません。

## 完了をどう表現するか

第9章の`QueryResult`は、DDL文の完了を`command_tag: Option<&'static str>`という、種類の名前だけの文字列で表していました。
`INSERT`、`UPDATE`、`DELETE`は、それに加えて「何行に影響したか」を報告したいところです。
`command_tag`を`Option<String>`に変え、影響行数を持つ完了を作る関数を追加します。

```rust
fn command_with_count(tag: &'static str, count: usize) -> Self {
    QueryResult {
        schema: Schema::new(Vec::new()),
        rows: Vec::new(),
        command_tag: Some(format!("{tag} {count}")),
    }
}
```

`INSERT INTO users VALUES (1, 'Alice')`を実行すると、REPLには`INSERT 1`とだけ表示されます。
psqlは`INSERT 0 1`のように、行の挿入先を表す2つ目の数値(OID、現在はほぼ常に0)を持ちますが、このクレートにOIDに相当する概念は無いため、この章では「文の種類」と「影響行数」だけを持つ、より単純な形式に決めます。

## テストで確認する

`executor`モジュールには、SeqScan、Filter、Projection、Insert、Update、Deleteそれぞれの単体テストを追加しました。
`database`モジュールには、`INSERT`→`SELECT`→`UPDATE`→`SELECT`→`DELETE`→`SELECT`という一連の流れを1つのテストとして確認するものも加えています。

```rust
#[test]
fn insert_select_update_delete_round_trip() {
    let mut db = users_db();
    db.execute("INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob')")
        .unwrap();
    assert_eq!(db.execute("SELECT * FROM users").unwrap().rows().len(), 2);

    db.execute("UPDATE users SET name = 'Alicia' WHERE id = 1")
        .unwrap();
    let updated = db.execute("SELECT name FROM users WHERE id = 1").unwrap();
    assert_eq!(
        updated.rows()[0].values(),
        &[Value::Text("Alicia".to_string())]
    );

    db.execute("DELETE FROM users WHERE id = 2").unwrap();
    let remaining = db.execute("SELECT id FROM users").unwrap();
    assert_eq!(remaining.rows().len(), 1);
    assert_eq!(remaining.rows()[0].values(), &[Value::BigInt(1)]);
}
```

NOT NULL違反やWHEREの三値論理も、直接それを狙ったテストで確認しています。

```rust
#[test]
fn select_where_drops_unknown_rows() {
    // `name`が`NULL`の行は、`name = 'Alice'`がUNKNOWNになるため落ちる
    // (FALSEになる場合と同じ扱い)。
    let mut db = users_db();
    db.execute("INSERT INTO users (id) VALUES (1)").unwrap();
    db.execute("INSERT INTO users VALUES (2, 'Alice')").unwrap();
    let result = db
        .execute("SELECT id FROM users WHERE name = 'Alice'")
        .unwrap();
    assert_eq!(result.rows().len(), 1);
    assert_eq!(result.rows()[0].values(), &[Value::BigInt(2)]);
}
```

Golden Testは、これまで1ファイルにつき1文しか置けませんでした(第3章)。
`CREATE TABLE`と`INSERT`をまたぐ流れは、複数文を実行できる単体テストの役目として書き分ける、という方針を第9章で採っています。
しかしこの章のDMLは、`CREATE TABLE`で作ったテーブルに`INSERT`してから`SELECT`で覗く、という組み合わせを抜きにして単独では意味を持ちません。
そこで、Golden Testのランナーを拡張し、`.sql`ファイルが`;`区切りの複数文を持てるようにしました。

```rust
fn run_sql(sql: &str) -> String {
    let mut db = temp_db();
    sql.split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
        .map(|statement| match db.execute(statement) {
            Ok(result) => result.to_string(),
            Err(e) => format!("ERROR: {e}"),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
```

すべての文を同じ`Database`で順に実行し、各文の結果を空行区切りで連結したものを期待値と突き合わせます。
`tests/golden/017_update.sql`は、`CREATE TABLE`、`INSERT`、`UPDATE`、`SELECT`の4文を1ファイルに書き、対応する`.expected`が4つの結果を並べたものになります。

```console
$ cargo test
running 164 tests
test database::tests::insert_select_update_delete_round_trip ... ok
test database::tests::select_where_drops_unknown_rows ... ok
test database::tests::update_set_right_hand_side_sees_the_pre_update_row ... ok
test executor::tests::update_leaves_the_table_untouched_when_a_row_fails_validation ... ok
...
test result: ok. 164 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

running 1 test
test golden_tests_pass ... ok
```

## SQLiteとのDifferential Testを導入する

ここまでのテストは、すべて「minidbが期待する答えを、minidb自身のコードを書いた人間が決める」という形をしています。
`WHERE name = 'Alice'`が`NULL`の行を落とすべきだという判断も、`assert_eq!`に書いた期待値も、結局は同じ人間の頭から出てきたものです。
実装のバグと期待値のバグが同じ勘違いから生まれていれば、テストは通ったまま、両方とも間違っているという状況になりえます。

この勘違いに気づく方法の1つが、同じSQLを、実装も歴史も無関係な別のデータベースに投げて、答えを見比べることです。
SQLiteは1つのファイル(あるいはメモリ上)で完結する軽量なRDBMSで、Rustの`rusqlite`クレートを使えばプロセス内に組み込めます。
`Cargo.toml`に`rusqlite`を`bundled`機能付きのdev-dependencyとして追加し、`tests/differential.rs`にDifferential Testのハーネスを作ります。

```toml
[dev-dependencies]
rusqlite = { version = "0.40.1", features = ["bundled"] }
```

`bundled`機能は、SQLite本体のCソースコードをビルド時に同梱コンパイルする指定です。
開発環境にSQLiteの共有ライブラリが入っているかどうかに関係なく、`cargo test`がそのまま動きます。

比較の基本形は、同じ`setup`(`CREATE TABLE`、`INSERT`、`UPDATE`、`DELETE`)をminidbと`rusqlite`の両方に流し、最後に1本の`SELECT`を実行して結果を突き合わせる、というものです。

```rust
fn assert_same_result(setup: &[&str], query: &str) {
    let minidb_rows = run_minidb(setup, query);
    let sqlite_rows = run_sqlite(setup, query);
    assert_eq!(
        minidb_rows, sqlite_rows,
        "minidbとSQLiteの結果が一致しません: setup={setup:?}, query={query:?}"
    );
}
```

`setup`のSQL文字列は、`rusqlite`側にも一切変更せずそのまま渡せます。
minidbが対応する`CREATE TABLE`、`INSERT`、`UPDATE`、`DELETE`の構文は、SQLiteが受け入れる構文の範囲に収まっているからです。
`BIGINT`や`TEXT`という型名も、SQLiteでは「型名の文字列に`INT`を含めば整数の扱いにする」といったゆるい型付け(型アフィニティ)のもとで問題なく通ります。

比較そのものは、2つのRDBMSが返す値をそれぞれ正規化してから行います。

```rust
fn format_minidb_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::Text(s) => s.clone(),
    }
}

fn format_sqlite_value(value: ValueRef) -> String {
    match value {
        ValueRef::Null => "NULL".to_string(),
        ValueRef::Integer(n) => n.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(bytes) => String::from_utf8_lossy(bytes).to_string(),
        ValueRef::Blob(_) => panic!("このサブセットにBLOBは存在しないはずです"),
    }
}
```

`NULL`はどちらも`"NULL"`という文字列に、整数は10進数の文字列表現に、文字列はそのままの内容にそろえます。
`BOOLEAN`列は、この正規化の対象から外しています。
SQLiteは真偽値専用の型を持たず、内部的には`0`、`1`という整数として扱うため、`true`、`false`という文字列を返すminidbとは同じ正規化関数では比較できません。
この章のDifferential Testは、両者の表示が素直に一致する範囲(`BIGINT`、`TEXT`、`NULL`)に限定し、`BOOLEAN`列を含む比較は今後の課題として残します。

`ORDER BY`はまだ構文解析器が受理しません(第21章で追加します)。
そのため、比較する`SELECT`はどれも`ORDER BY`を持たず、Sequential Scanが挿入順を保つminidbと、単純な全表走査ではおおむね挿入順(rowid順)で行を返すSQLiteが、たまたま同じ順序になるという前提に乗っています。
索引やJoinが絡む複雑なクエリではこの前提が崩れるので、`ORDER BY`が使えるようになった章で、テストケースにも付け直す必要があります。

三値論理の扱いをminidbとSQLiteで突き合わせるテストは、次のようになりました。

```rust
#[test]
fn where_with_null_drops_unknown_rows() {
    assert_same_result(
        &[
            "CREATE TABLE users (id BIGINT NOT NULL, name TEXT)",
            "INSERT INTO users (id) VALUES (1)",
            "INSERT INTO users VALUES (2, 'Alice')",
        ],
        "SELECT id FROM users WHERE name = 'Alice'",
    );
}
```

`cargo test --test differential`を実行すると、`CREATE`、複数行の`INSERT`、列名指定、`WHERE`の三値論理、`UPDATE`、`DELETE`、算術式を含む7ケースがすべて通ります。

```console
$ cargo test --test differential
running 7 tests
test create_insert_select ... ok
test multi_row_insert_with_where ... ok
test insert_with_explicit_columns_fills_omitted_columns_with_null ... ok
test where_with_null_drops_unknown_rows ... ok
test update_changes_matching_rows ... ok
test delete_removes_matching_rows ... ok
test arithmetic_and_string_comparison_in_where ... ok

test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

## 壊して確認する

Filter演算子の三値論理の判定を、実際に壊して確認します。
「TRUEの行だけを残す」を、「FALSEでない行を残す」に書き換えると、一見同じことを言っているように見えます。

```rust
// if matches!(eval_expr(predicate, functions, Some(&row))?, Value::Boolean(true)) {
if !matches!(
    eval_expr(predicate, functions, Some(&row))?,
    Value::Boolean(false)
) {
    kept.push(tuple);
}
```

この状態で`select_where_drops_unknown_rows`を実行すると、期待どおり赤くなります。

```console
thread 'database::tests::select_where_drops_unknown_rows' panicked at src/database.rs:707:9:
assertion `left == right` failed
  left: 2
 right: 1
```

`name`が`NULL`の行の`name = 'Alice'`は、`FALSE`ではなく`UNKNOWN`(`Value::Null`)です。
「`FALSE`でない」という条件は`UNKNOWN`にも成り立ってしまうため、壊した`filter`はこの行を「一致した」側に誤って残します。
`WHERE id FROM users`は本来1行(`id = 2`)だけを返すべきところ、`id = 1`の行(`name`が`NULL`)まで一緒に返してしまい、テストの`left: 2`という行数がその混入を示しています。

同じ壊れ方を、SQLiteとのDifferential Testも独立に検出します。

```console
thread 'where_with_null_drops_unknown_rows' panicked at tests/differential.rs:32:5:
assertion `left == right` failed: minidbとSQLiteの結果が一致しません: ...
  left: [["1"], ["2"]]
 right: [["2"]]
```

`left`(minidb)が`UNKNOWN`の行を混入させて2行返しているのに対し、`right`(SQLite)は本来の1行だけを返しています。
`select_where_drops_unknown_rows`は「minidbの実装が、minidbのテストが期待する答えからずれた」ことしか教えてくれませんが、このDifferential Testは「minidbの答えが、SQLiteという独立した実装の答えからもずれた」ことまで教えてくれます。
自分で書いたテストと期待値が両方とも同じ勘違いに基づいていたら検出できない種類のバグを、この章で導入したDifferential Testは検出できる位置に立っています。

`if !matches!(..., Value::Boolean(false))`を元の`if matches!(..., Value::Boolean(true))`に戻すと、両方のテストが再び緑に戻ります。

## 第1部の到達点

`CREATE TABLE`でテーブルを作り、`INSERT`で行を入れ、`SELECT`で`WHERE`と`*`を使って読み出し、`UPDATE`で書き換え、`DELETE`で消す。
このひとまとまりの操作が、REPLからそのまま動きます。

```console
minidb> CREATE TABLE users (id BIGINT NOT NULL, name TEXT);
CREATE TABLE
minidb> INSERT INTO users VALUES (1, 'Alice'), (2, 'Bob');
INSERT 2
minidb> SELECT * FROM users;
id | name
---------
1 | Alice
2 | Bob
(2 rows)
minidb> UPDATE users SET name = 'Carol' WHERE id = 1;
UPDATE 1
minidb> DELETE FROM users WHERE id = 2;
DELETE 1
minidb> SELECT * FROM users;
id | name
---------
1 | Carol
(1 row)
```

第1章のロードマップが「インメモリの最小実装」と呼んでいたものが、この章で完成しました。
再起動すればテーブルもデータもすべて消えますが、動いている間はSQLだけで一通りのことができる、小さなデータベースです。
ディスクに書き出して永続化する仕事は、第2部で始めます。

## 演習問題

### 必須課題

1. `executor::update`と`executor::delete`は、`predicate`が`None`のとき全行を対象にします。`UPDATE users SET name = 'X'`のようにWHEREを省略した`UPDATE`が、本当に全行を書き換えることを確認する単体テストを追加してください。
2. `INSERT INTO users (id, id) VALUES (1, 2)`のように、同じ列名を`INSERT`の列リストに2回書いた場合の挙動を確認してください。`expand_to_schema`のどの分岐がこれを検出しているか、コードを読んで説明したうえで、対応するテストが無ければ追加してください。
3. `SELECT id, * FROM users`のように、`*`と通常の式を同じ対象式リストに混ぜて書けることを確認してください。`resolve_items`の実装がこれをどう扱っているかを読み、対応するテストが無ければ追加してください。

### 発展課題

1. この章のDifferential Testは`BOOLEAN`列を比較の対象から外しています。SQLiteの`0`/`1`とminidbの`true`/`false`を、列の宣言型を見て正規化する仕組みを設計し、実装してください。`CREATE TABLE`のSQLからどうやって列の型を取り出すか(SQLiteに問い合わせるか、テスト側で別途宣言するか)を検討する必要があります。
2. `UPDATE`と`DELETE`は、対象の行を`Vec<Tuple>`の添字で直接書き換えています。行数が多いテーブルに対して`WHERE`無しの`DELETE`を繰り返し実行するベンチマークを書き、`*table.rows_mut() = kept`という書き戻し方の計算量を確認してください。第2部でHeap Fileに置き換わったとき、この書き戻し方がどう変わるべきかを考察してください。
3. `INSERT INTO users SELECT id, name FROM other_users`のような、`VALUES`の代わりに`SELECT`の結果を挿入元にする構文(`INSERT ... SELECT`)は、このSQLサブセットにまだありません。`InsertStatement`と`executor::insert`をどう変更すればこの構文に対応できるか、設計だけ考えてみてください(実装は必須ではありません)。
