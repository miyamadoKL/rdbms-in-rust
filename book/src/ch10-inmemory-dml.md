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

1. **WHEREはTRUEの行だけを採用する**: SQLの`WHERE`は三値論理で評価するため、`FALSE`になった行はもちろん、`UNKNOWN`(`NULL`)になった行も結果から落とす。`BOOLEAN`でも`NULL`でもない値(`WHERE 1`など)は、黙って「一致しなかった」側に丸めず、エラーにする
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

`WHERE`の評価結果を「一致したかどうか」という`bool`へ変換する部分は、`Filter`だけでなく`Update`や`Delete`にも共通して必要です。
`predicate`には`WHERE id = 1`のような`BOOLEAN`を返す式が書かれているのが普通ですが、`WHERE 1`のように`BIGINT`を返す式を書き誤ることもありえます。
そのような値を、`TRUE`でないというだけで黙って「一致しなかった」側に丸めてしまうと、書き誤りに気づけないまま0行という結果を正常応答として返してしまいます。
そこで、この変換は`predicate_matches`という1つの関数にまとめ、`filter`、`update`、`delete`の3箇所から共通して呼びます。

```rust
fn predicate_matches(value: Value) -> DbResult<bool> {
    match value {
        Value::Boolean(true) => Ok(true),
        Value::Boolean(false) | Value::Null => Ok(false),
        other => {
            // `Value::Boolean`と`Value::Null`は直前の分岐で処理済みなので、
            // ここに来る`other`は必ず`data_type()`が`Some`を返す値(`BigInt`/`Text`)
            // である。`Option`を`{:?}`でそのまま表示すると`Some(BigInt)`のように
            // Rustの内部表現が利用者に漏れてしまうため、`unwrap`してSQLの型名
            // だけを見せる。
            let data_type = other
                .data_type()
                .expect("BooleanとNullは既に処理済みなのでdata_type()は必ずSomeを返す");
            Err(DbError::Eval(format!(
                "WHERE句はBOOLEANまたはNULLを返す式である必要があります: {data_type}が渡されました"
            )))
        }
    }
}
```

`predicate_matches`の分岐は3種類です。
`Boolean(true)`だけが一致で、`Boolean(false)`と`Value::Null`(三値論理のUNKNOWN)はどちらも一致しません。
「UNKNOWNの行を、一致しなかった行と同じように扱う」という三値論理の規則は、この2つ目の分岐が担っています。
`BIGINT`や`TEXT`のような`BOOLEAN`でもNULLでもない値は、3つ目の分岐で`DbError::Eval`にします。
`WHERE 1`のような式は、この分岐によって「0行がマッチした」という静かな誤答ではなく、実行時エラーとしてはっきり報告されます。
エラーメッセージにSQLの型名だけを見せるため、`DataType`には`Display`実装(`BIGINT`/`TEXT`/`BOOLEAN`という表示名)を用意し、`Option<DataType>`をそのまま`{:?}`で表示することは避けています。

```console
minidb> SELECT id FROM users WHERE 1;
エラー: 評価エラー: WHERE句はBOOLEANまたはNULLを返す式である必要があります: BIGINTが渡されました
```

Filter演算子は、`守るべき不変条件`の1番目を、この`predicate_matches`を使ってそのままコードにしただけです。

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
        let value = eval_expr(predicate, functions, Some(&row))?;
        if predicate_matches(value)? {
            kept.push(tuple);
        }
    }
    Ok(kept)
}
```

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

出力列の型と`nullable`は、単純な列参照であれば`table_schema`の定義をそのままコピーします。
これなら、行が1件も無いテーブルに対する`SELECT id FROM empty_table`でも、列の型を正確に決められます。

`id + 1`のような計算結果の型を決めるには、もう一段考える必要があります。
実際に1行評価してみて、その結果の`Value`から`data_type()`を読む案も考えられますが、この案には見落としがあります。
`CREATE TABLE t (x BIGINT); INSERT INTO t VALUES (NULL), (1);`のように、1行目の`x`が`NULL`であるテーブルを考えてください。
`SELECT abs(x) FROM t`を実行するとき、1行目を評価した結果は`Value::Null`で、`Value::Null`はどの`DataType`にも属さないため`data_type()`は`None`を返します。
ここで安易に`TEXT`を仮の型として採用すると、出力列の型は`TEXT`のまま固定されてしまいます。
2行目の`abs(1)`が返す`Value::BigInt(1)`を`Tuple::new`に渡した瞬間、宣言した列の型(`TEXT`)と実際の値の型(`BigInt`)が食い違うという`DbError::SchemaMismatch`が飛んできます。
1行目が`NULL`だったというだけの理由で、2行目以降の正しい計算結果までエラーにしてしまうわけです。

この章の`project`は、行を1行も評価せずに出力列の型を決める`infer_type`という関数を使い、この問題を避けます。

```rust
fn infer_type(expr: &Expr, schema: &Schema, functions: &FunctionRegistry) -> DbResult<DataType> {
    match expr {
        Expr::IntLiteral { .. } => Ok(DataType::BigInt),
        Expr::StringLiteral { .. } => Ok(DataType::Text),
        Expr::BoolLiteral { .. } => Ok(DataType::Boolean),
        // `NULL`単体の型については、この関数のドキュメントコメントを参照。
        Expr::NullLiteral { .. } => Ok(DataType::Text),
        Expr::ColumnRef { name, .. } => schema
            .column(name)
            .map(|column| column.data_type)
            .ok_or_else(|| DbError::Eval(format!("列'{name}'が見つかりません"))),
        Expr::UnaryOp { op, expr, .. } => match op {
            UnaryOperator::Negate => infer_type(expr, schema, functions),
            UnaryOperator::Not => Ok(DataType::Boolean),
        },
        Expr::BinaryOp { op, .. } => match op {
            // 両辺の型が正しいかどうかの検査自体は`eval_expr`の役目であり、
            // ここでは演算子の種類だけから出力の型を決める。
            BinaryOperator::Add
            | BinaryOperator::Subtract
            | BinaryOperator::Multiply
            | BinaryOperator::Divide => Ok(DataType::BigInt),
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
            | BinaryOperator::And
            | BinaryOperator::Or => Ok(DataType::Boolean),
        },
        Expr::IsNull { .. } => Ok(DataType::Boolean),
        Expr::Cast { type_name, .. } => DataType::from_sql_name(&type_name.name)
            .ok_or_else(|| DbError::Eval(format!("未知の型名です: {}", type_name.name))),
        Expr::FunctionCall { name, .. } => functions.return_type(name),
        Expr::Paren { expr, .. } => infer_type(expr, schema, functions),
    }
}
```

`infer_type`は式のASTと`table_schema`だけを見て、行の値には一度も触れません。
リテラルはそのリテラルが表す型を、列参照は`schema`に定義された型を、算術演算(`+ - * /`、単項`-`)は`BigInt`を、比較演算や論理演算、`IS [NOT] NULL`は`Boolean`を、`CAST`は`type`が指す型を、それぞれ返します。
関数呼び出しは、`FunctionRegistry`に登録された戻り値の型(`functions.return_type(name)`)をそのまま返すので、`abs(x)`の型は`abs`の戻り値の型(`BigInt`)だけから決まり、`x`の1行目が`NULL`かどうかには依存しません。
先ほどの`SELECT abs(x) FROM t`は、この章では`x`が`NULL`の行を含んでいても、常に`BigInt`列として実行できます。
`NULL`リテラル単体だけは例外で、`Value::Null`がどの`DataType`にも属さないのと同じ理由で型を持たないため、`TEXT`を仮の型として割り当てます。

計算結果の`nullable`は常に`true`にしています。
行ごとに`NULL`になったりならなかったりしうる式の`nullable`を、静的な型推論だけで`false`と決め打ってしまうと、実際に`NULL`が出てきた行で`Tuple::new`のスキーマ検査がその`NULL`を不当に拒否してしまうからです。
列参照そのものであれば`table_schema`の`nullable`をそのまま使えるので、この問題は起きません。
名前解決を伴う本格的な型検査は、第17章のBinderが引き継ぎます。

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
            Some(pred) => predicate_matches(eval_expr(pred, functions, Some(&row))?)?,
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
            Some(pred) => predicate_matches(eval_expr(pred, functions, Some(&row))?)?,
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
文を分ける実装は、`;`という文字だけを見て`str::split`する素朴な形にはしません。
`SELECT 'a;b'`のような文字列リテラルや、`-- a;b`のようなコメントの内側にも`;`は現れるため、その`;`まで文の区切りとして誤認してしまうからです。
そこで`split_statements`は、字句解析器の`tokenize`を一度通し、`TokenKind::Semicolon`のトークンだけを区切りとして扱います。

```rust
fn split_statements(sql: &str) -> Vec<&str> {
    let tokens =
        tokenize(sql).unwrap_or_else(|e| panic!("golden testのSQLをtokenizeできません: {e}"));

    let mut statements = Vec::new();
    let mut start = 0usize;
    for token in &tokens {
        match token.kind {
            TokenKind::Semicolon => {
                let text = sql[start..token.span.start].trim();
                if !text.is_empty() {
                    statements.push(text);
                }
                start = token.span.end;
            }
            TokenKind::Eof => {
                let text = sql[start..token.span.start].trim();
                if !text.is_empty() {
                    statements.push(text);
                }
            }
            _ => {}
        }
    }
    statements
}
```

各`Token`は元のソース上のバイト範囲を`Span`として持っているので、`Semicolon`トークンに出会うたびに、直前の区切り位置からそのトークンの開始位置までを`sql`からそのままスライスします。
文字列リテラルやコメントの中身がどんな文字を含んでいても、`tokenize`がその範囲を1つのトークン(または読み飛ばすコメント)として扱う以上、区切りの`TokenKind::Semicolon`として誤検出されることはありません。
`run_sql`は、この`split_statements`が返した文をそのまま`Database`に順に流し込みます。

```rust
fn run_sql(sql: &str) -> String {
    let mut db = temp_db();
    split_statements(sql)
        .into_iter()
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
`tests/golden/019_semicolon_in_string_and_comment.sql`は、この`split_statements`が守るべき規則そのものを回帰テストにしたものです。

```sql
-- comment with a semicolon; embedded here
SELECT 'a;b'; -- trailing line comment; also has one
/* block comment
   with a semicolon; embedded here too */
SELECT 'c;d';
```

行コメント、ブロックコメント、文字列リテラルのどれもが`;`を内側に含みますが、`split_statements`はこれらを区切りと誤認せず、`SELECT 'a;b'`と`SELECT 'c;d'`という2文に正しく分けます。

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

比較そのものは、2つのRDBMSが返す値を、それぞれ型タグ付きの中間表現へ変換してから行います。
最初に思いつく素朴な方法は、`.to_string()`のような文字列化を経由してから比較することですが、この方法には見落としがあります。
`Value::Null`と、文字列の中身がたまたま`"NULL"`である`Value::Text("NULL".to_string())`は、どちらも文字列化すると同じ`"NULL"`になってしまいます。
整数`1`と文字列`"1"`も同様に、どちらも`"1"`という同じ文字列に潰れます。
異なる型の値が同じ文字列表現を持つことがある以上、文字列化してからの比較は、この2つを取り違えたまま「一致した」と誤判定しかねません。

そこでこの章のDifferential Testは、値を文字列へ変換する代わりに、型ごとに別のバリアントを持つ`DiffValue`という列挙型に変換してから比較します。

```rust
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DiffValue {
    Null,
    Boolean(bool),
    Integer(i64),
    Text(String),
}
```

`Null`と`Text("NULL".to_string())`、`Integer(1)`と`Text("1".to_string())`は、`DiffValue`では異なるバリアントなので、`PartialEq`による比較で取り違えることがありません。

`BOOLEAN`列の扱いにも、同じ理由で一工夫が要ります。
SQLiteは真偽値専用の型を持たず、`BOOLEAN`列も内部的には`0`または`1`という整数として保持します。
この`0`や`1`を値だけ見て「`BOOLEAN`だろう」と推測するのは、`BIGINT`列にたまたま`0`や`1`が入っている場合と区別がつかず危険です。
そこでこの章のDifferential Testは、値からの推測に頼らず、minidb側で`query`を実行して得られる`QueryResult`のスキーマから、SELECTした各列の`DataType`を求め、その列型が`DataType::Boolean`だと分かっている列でだけ、SQLiteの整数`0`や`1`をそれぞれ`DiffValue::Boolean(false)`、`DiffValue::Boolean(true)`に読み替えます。

結果の比較は、行の順序を無視した比較にしています。

```rust
fn assert_same_result(setup: &[&str], query: &str) {
    let (mut minidb_rows, column_types) = run_minidb(setup, query);
    let mut sqlite_rows = run_sqlite(setup, query, &column_types);

    minidb_rows.sort();
    sqlite_rows.sort();

    assert_eq!(
        minidb_rows, sqlite_rows,
        "minidbとSQLiteの結果が一致しません: setup={setup:?}, query={query:?}"
    );
}
```

`ORDER BY`はまだ構文解析器が受理しません(第21章で追加します)。
`ORDER BY`の無い`SELECT`の行順序はSQLの意味論上未規定なので、Sequential Scanが挿入順を保つminidbと、SQLiteの実装詳細(たいてい挿入順やrowid順)が、行の集合として同じでも順序だけ食い違うことがあります。
順序のまま`Vec`同士を比較していると、この食い違いだけでテストが偽の失敗をします。
`minidb_rows`と`sqlite_rows`をそれぞれ`.sort()`してから`assert_eq!`することで、行の集合としての一致(多重集合としての一致)だけを見るようにし、順序の違いを比較の対象から外しています。
ソートは重複行の個数を潰さないため、`(1, 'a'), (1, 'a'), (2, 'b')`という結果は`(1, 'a'), (2, 'b'), (1, 'a')`とは一致しても`(1, 'a'), (2, 'b')`とは一致しません。

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

`Value::Null`と文字列`'NULL'`、整数`1`と文字列`'1'`が取り違えられないことも、それぞれ専用のテストで確認しています。

```rust
#[test]
fn null_is_not_confused_with_the_text_null() {
    assert_same_result(
        &[
            "CREATE TABLE t (id BIGINT NOT NULL, label TEXT)",
            "INSERT INTO t (id) VALUES (1)",
            "INSERT INTO t VALUES (2, 'NULL')",
        ],
        "SELECT id, label FROM t",
    );
}
```

`id = 1`の行は`label`が`NULL`、`id = 2`の行は`label`が文字列`'NULL'`です。
`DiffValue`が型ごとにバリアントを分けているおかげで、この2つの行がminidbとSQLiteのどちらでも取り違えられないことを、このテストが確認します。

`cargo test --test differential`を実行すると、`CREATE`、複数行の`INSERT`、列名指定、`WHERE`の三値論理、`UPDATE`、`DELETE`、算術式、行順序の無視、多重集合としての一致、`NULL`と`BOOLEAN`列の型の取り違え防止を含む12ケースがすべて通ります。

```console
$ cargo test --test differential
running 12 tests
test create_insert_select ... ok
test multi_row_insert_with_where ... ok
test insert_with_explicit_columns_fills_omitted_columns_with_null ... ok
test where_with_null_drops_unknown_rows ... ok
test update_changes_matching_rows ... ok
test delete_removes_matching_rows ... ok
test arithmetic_and_string_comparison_in_where ... ok
test multi_row_insert_returns_rows_in_any_order ... ok
test bag_comparison_preserves_duplicate_row_counts ... ok
test null_is_not_confused_with_the_text_null ... ok
test integer_is_not_confused_with_its_text_representation ... ok
test boolean_column_is_reconciled_against_sqlite_zero_one ... ok

test result: ok. 12 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

## 壊して確認する

`predicate_matches`の三値論理の判定を、実際に壊して確認します。
「`TRUE`の行だけをマッチとする」を、「`FALSE`でない行をマッチとする」に書き換えると、一見同じことを言っているように見えます。

```rust
fn predicate_matches(value: Value) -> DbResult<bool> {
    match value {
        Value::Boolean(false) => Ok(false),
        _ => Ok(true), // UNKNOWNの特別扱いを消した
    }
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
「`FALSE`でない」という条件は`UNKNOWN`にも成り立ってしまうため、壊した`predicate_matches`はこの行を「一致した」側に誤って残します。
`SELECT id FROM users WHERE name = 'Alice'`は本来1行(`id = 2`)だけを返すべきところ、`id = 1`の行(`name`が`NULL`)まで一緒に返してしまい、テストの`left: 2`という行数がその混入を示しています。

同じ壊れ方を、SQLiteとのDifferential Testも独立に検出します。

```console
thread 'where_with_null_drops_unknown_rows' panicked at tests/differential.rs:32:5:
assertion `left == right` failed: minidbとSQLiteの結果が一致しません: ...
  left: [[Integer(1)], [Integer(2)]]
 right: [[Integer(2)]]
```

`left`(minidb、ソート済み)が`UNKNOWN`の行を混入させて2行返しているのに対し、`right`(SQLite、ソート済み)は本来の1行だけを返しています。
`select_where_drops_unknown_rows`は「minidbの実装が、minidbのテストが期待する答えからずれた」ことしか教えてくれませんが、このDifferential Testは「minidbの答えが、SQLiteという独立した実装の答えからもずれた」ことまで教えてくれます。
自分で書いたテストと期待値が両方とも同じ勘違いに基づいていたら検出できない種類のバグを、この章で導入したDifferential Testは検出できる位置に立っています。

`predicate_matches`を元の実装(`Boolean(true)`だけを`Ok(true)`、`Boolean(false)`と`Null`を`Ok(false)`、それ以外をエラーにする3分岐)に戻すと、両方のテストが再び緑に戻ります。

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

1. `src/database.rs`の`update_without_where_changes_every_row`と`src/executor.rs`の`delete_without_predicate_removes_every_row`を読んでください。どちらも`WHERE`を省略した`UPDATE`や`DELETE`が全行を対象にすることを確認していますが、確認の粒度が異なります(片方は`Database::execute`を通した結果の行数、もう片方は`executor::delete`単体の戻り値と`table`の中身)。この2つのテストが同じ主張を別の層で検証していると言えるのはなぜか、そしてこの章の「全行を評価し終えてから一括で書き込む」というAll-or-Nothingの順序が、`predicate`が`None`の場合(検査すべき`WHERE`が無い場合)にどう関わるかを説明してください。
2. `INSERT INTO users (id, id) VALUES (1, 2)`のように、同じ列名を`INSERT`の列リストに2回書いた場合の挙動を確認してください。`expand_to_schema`のどの分岐がこれを検出しているか、コードを読んで説明したうえで、対応するテストが無ければ追加してください。
3. `SELECT id, * FROM users`のように、`*`と通常の式を同じ対象式リストに混ぜて書けることを確認してください。`resolve_items`の実装がこれをどう扱っているかを読み、対応するテストが無ければ追加してください。

### 発展課題

1. この章のDifferential Testは、`setup`と`query`がどちらも成功する場合しか比較していません。`SELECT 1 / 0`のようにminidbが`DbError::Eval`を返すべき入力を、SQLiteに投げるとどうなるか調べてください。SQLiteは整数の`0`除算をエラーにせず`NULL`を返し、`i64`の範囲を超える加算もエラーにせず浮動小数点数へ黙って昇格させます。両者のエラー挙動の違いを比較するテストの枠組みをどう設計すべきか(`assert_same_result`をそのまま使えるか、エラーになったかどうかだけを比較する専用の関数が要るか)を考えてください。
2. `UPDATE`と`DELETE`は、対象の行を`Vec<Tuple>`の添字で直接書き換えています。行数が多いテーブルに対して`WHERE`無しの`DELETE`を繰り返し実行するベンチマークを書き、`*table.rows_mut() = kept`という書き戻し方の計算量を確認してください。第2部でHeap Fileに置き換わったとき、この書き戻し方がどう変わるべきかを考察してください。
3. `INSERT INTO users SELECT id, name FROM other_users`のような、`VALUES`の代わりに`SELECT`の結果を挿入元にする構文(`INSERT ... SELECT`)は、このSQLサブセットにまだありません。`InsertStatement`と`executor::insert`をどう変更すればこの構文に対応できるか、設計だけ考えてみてください(実装は必須ではありません)。
