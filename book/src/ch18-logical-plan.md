# 第18章 関係代数とLogical Plan

`SELECT name FROM users WHERE id = 42`を実行するとき、`WHERE`と`SELECT`のどちらを先に評価するべきでしょうか。

答えるのに時間はかかりません。
テーブル全体を読み、`id = 42`で絞り込んでから、残った行の`name`だけを取り出す。
それ以外の順番では、まだ絞り込んでいない行にまで`name`の取り出しを試みることになり、無駄です。
`WHERE`が先、`SELECT`の対象式はその後。
この順序は自明です。

問題は、その自明な順序が、このクレートのどこに書いてあるかです。

## 計算の順序をどこが決めているか

`src/database.rs`にある、第17章までの`Database::execute_select_with_from`を見ると、答えが見つかります。

```rust
fn execute_select_with_from(&self, select: &BoundSelect) -> DbResult<QueryResult> {
    let table = &select.tables[0];

    let scanned = match &self.backend {
        Backend::Memory { storage, .. } => { /* ... */ executor::seq_scan(mem_table) }
        Backend::Disk { storage } => executor::storage_seq_scan(storage, table.table_id, &table.schema)?,
    };

    let filtered = match &select.predicate {
        Some(predicate) => executor::filter(&table.schema, &self.functions, scanned, predicate)?,
        None => scanned,
    };
    let (schema, rows) =
        executor::project(&table.schema, &self.functions, &filtered, &select.projection)?;

    Ok(QueryResult { schema, rows, command_tag: None })
}
```

Sequential Scanを`scanned`に読み込み、`filtered`へ絞り込み、`project`で射影する。
この3行の並びそのものが「`WHERE`が先、`SELECT`の対象式はその後」という順序の唯一の記録です。
`BoundSelect`という型を見ても、`tables`、`projection`、`predicate`という3つのフィールドが並んでいるだけで、どれを先に評価するべきかは書かれていません。
順序は`execute_select_with_from`という関数の中身、つまりRustのコードの実行順そのものに埋め込まれています。

`FROM`を伴わない`SELECT`には、これとは別に`execute_select_without_from`という関数がありました。
`SELECT 1 + 1`のような、テーブルを持たない`SELECT`のために、列を持たない暗黙の1行を用意し、`WHERE`があればその1行に対して`filter`をかけ、一致すれば射影式を評価するという、`execute_select_with_from`とほぼ同じ処理をもう一度書き下ろす関数です。
`Sequential Scan→Filter→Projection`という同じ順序が、`FROM`の有無という枝分かれのために、2つの関数として重複していました。

`Binder`(第17章)は、名前を確かめる仕事と型を確かめる仕事を1つの層にまとめました。
この章が向き合うのは、その次の仕事です。
`BoundSelect`はすでに「`users`の`id`列」のような意味を確定させていますが、「`WHERE`を先に評価し、その結果に`SELECT`の対象式を適用する」という計算の手順そのものは、まだどの型にも現れていません。
手順は`execute_select_with_from`という1つの関数の、コードの並び順という形でしか存在しないのです。

## 関係代数の演算子を木として組み立てる

SQLが表す計算は、**関係代数**という代数の演算に対応づけられます。
関係代数は、表(リレーション)を入力に取り、表を出力する演算子の集まりです。
`WHERE`は**選択**(Selection、一致した行だけを残す)、`SELECT`の対象式は**射影**(Projection、列を絞り込んだり計算したりする)、`FROM`の1テーブルは**走査**(Scan、テーブル全体を読む)にそれぞれ対応します。

`SELECT`の各句を対応する演算子に置き換えると、`SELECT name FROM users WHERE id = 42`は次の木になります。

```text
Projection(name)
  └─ Filter(id = 42)
    └─ Scan(users)
```

木の親子関係が、そのまま計算の順序になります。
`Projection`は自分の子である`Filter`の結果を受け取ってから動き、`Filter`は自分の子である`Scan`の結果を受け取ってから動きます。
「`WHERE`が先、`SELECT`の対象式はその後」という順序は、もう`execute_select_with_from`のコードの並びに頼る必要がありません。
`Filter`が`Projection`の子であるという、木そのものの形として表現されています。

この演算子の木を**Logical Plan**と呼びます。
「Logical」という語が付くのは、この木がまだ「何を計算するか」しか決めていないからです。
`Scan(users)`は「`users`を走査する」ことだけを表し、それが全件読みになるのか、索引を使うのかは、この段階では決めません(索引はまだこのクレートに無いので、この区別は第23〜25章でB+TreeとIndex Scanが揃うまでは意味を持ちません)。
実行アルゴリズムまで確定した木は**Physical Plan**と呼ばれ、その変換は第19章の仕事です。

## `LogicalPlan`を設計する

演算子は7種類にとどめます。
新規ファイル`src/logical_plan.rs`を作り、`LogicalPlan`を次のように定義します。
`src/lib.rs`に`pub mod logical_plan;`を追加します。

```rust
pub enum LogicalPlan {
    Scan(ScanNode),
    Values(ValuesNode),
    Filter(FilterNode),
    Projection(ProjectionNode),
    Insert(InsertNode),
    Update(UpdateNode),
    Delete(DeleteNode),
}
```

`Scan`、`Values`、`Filter`、`Projection`は`SELECT`が使い、`Insert`、`Update`、`Delete`はそれぞれの文が使います。
`Join`、`Aggregate`、`Sort`、`Limit`に対応する構文は、`Parser`(第7章)にもまだありません。
`FROM`に書けるテーブルは1つ、`GROUP BY`もソートも`LIMIT`もこのクレートにはまだ無いので、対応する演算子を今作っても、どのSQL文からも組み立てられない死んだコードになります。
`enum`のバリアントを今のうちに増やさなかったのは、この章の設計判断です。
第21章で`Aggregate`、`Sort`、`Limit`が、第22章で`Join`が、それぞれ対応する構文と一緒に`LogicalPlan`へ加わります。

`Filter`、`Projection`、`Insert`、`Update`、`Delete`は、それぞれ1個の子(`input`)を持つ`struct`に情報をまとめてあります。
`Scan`、`Values`は子を持たない葉です。

```rust
pub struct ScanNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
}

pub struct ValuesNode {
    pub schema: Schema,
    pub rows: Vec<Vec<Expr>>,
}

pub struct FilterNode {
    pub input: Box<LogicalPlan>,
    pub predicate: BoundExpr,
}

pub struct ProjectionNode {
    pub input: Box<LogicalPlan>,
    pub projection: Vec<BoundSelectItem>,
}
```

`Values`は、`VALUES`が並べるリテラル式の行の並びを表す演算子です。
`INSERT INTO users (id, name) VALUES (1, 'a')`の`VALUES (1, 'a')`がそのまま1個の`Values`ノードになります。
`rows`の各要素はまだ評価していない`Expr`のままです。
`VALUES`は既存の行を参照する構文を持たないため、`Binder`が解決すべき列参照はそこに現れようが無く(第17章の`BoundInsert::rows`と同じ理由)、`Expr`のまま`Values`ノードへ持ち回ってよいことになります。

`FROM`を伴わない`SELECT`も、この`Values`を使って表現します。
`SELECT 1 + 1`は、列を1つも持たない行を1件だけ生成する`Values`を根の`Scan`の代わりに使うことで、`FROM`の有無による特別な演算子を増やさずに済みます。

```text
SELECT 1 + 1
  Projection(1 + 1)
    └─ Values(1 row)
```

`Insert`、`Update`、`Delete`も、それぞれ対象のテーブル(`table_id`、`schema`)と、演算に固有の情報を持ちます。

```rust
pub struct InsertNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub columns: Option<Vec<usize>>,
    pub input: Box<LogicalPlan>,
}

pub struct UpdateNode {
    pub table_id: TableId,
    pub table_name: String,
    pub schema: Schema,
    pub assignments: Vec<BoundAssignment>,
    pub predicate: Option<BoundExpr>,
    pub input: Box<LogicalPlan>,
}
```

`Insert`の子(`input`)は必ず`Values`です。
`Update`、`Delete`の子は、書き換えまたは削除の対象になるテーブルを表す`Scan`です。
`Update`、`Delete`にとって、この`Scan`は実際に別ステップとして走査されるわけではありません。
`executor::update`(第17章)は、「走査しながら`predicate`を評価し、一致した行だけ書き換える」という処理を1回の走査にまとめて行っており、`Scan`の結果をいったん`Vec<Tuple>`として実体化してから`Filter`と書き換えを別々に適用する形にはなっていません。
それでも`input`を`Scan`として木に残しているのは、「`Update`はどのテーブルに対する操作か」を演算子の親子関係として表現するためです。
1回の走査にまとめるか、`Filter`を独立した演算子として挟むかという実行方法の選択は、第19章のPhysical Planが決める領域であり、`LogicalPlan`の役目は「何に対する操作か」を確定させるところまでにとどめます。

## 各ノードが出力スキーマを答える

演算子の木を組み立てただけでは、各ノードが最終的にどんな列を返すのかが分かりません。
`LogicalPlan`には`output_schema`という、この問いにノード自身が答えるメソッドを持たせます。

```rust
pub fn output_schema(&self) -> Schema {
    match self {
        LogicalPlan::Scan(scan) => scan.schema.clone(),
        LogicalPlan::Values(values) => values.schema.clone(),
        LogicalPlan::Filter(filter) => filter.input.output_schema(),
        LogicalPlan::Projection(projection) => {
            projection_schema(&projection.input.output_schema(), &projection.projection)
        }
        LogicalPlan::Insert(_) | LogicalPlan::Update(_) | LogicalPlan::Delete(_) => Schema::new(Vec::new()),
    }
}
```

`Scan`、`Values`は自分が持つ`schema`をそのまま返します。
`Filter`は行を減らすだけで列構成を変えないため、子の`output_schema()`を素通しします。
`Insert`、`Update`、`Delete`は行を返さない文なので、列を1つも持たない空の`Schema`を返します(`Database::execute`が組み立てる`QueryResult`が、DDL、DML文に対して空の`Schema`を返すのと同じ約束事です)。

`Projection`だけは、子の出力を受け取って新しい`Schema`を組み立てる必要があります。
この組み立て方には規則があり、単純な列参照(`id`のような、式を伴わない列そのもの)は入力側の列定義(型、nullable)をそのまま引き継ぎ、`id + 1`のような計算結果の式は`item.expr.data_type()`(`Binder`が構築時に決めた型)を使います。

```rust
pub fn projection_schema(input_schema: &Schema, projection: &[BoundSelectItem]) -> Schema {
    let mut out_columns = Vec::with_capacity(projection.len());
    for item in projection {
        if let BoundExpr::ColumnRef { column_index, .. } = &item.expr {
            let mut column = input_schema.columns()[*column_index].clone();
            column.name = item.output_name.clone();
            out_columns.push(column);
            continue;
        }

        let data_type = item.expr.data_type().unwrap_or(DataType::Text);
        out_columns.push(Column::new(item.output_name.clone(), data_type, true));
    }
    Schema::new(out_columns)
}
```

この規則は、実は第17章の`executor::project`がすでに実装していたものです。
`project`は各行を実際に計算しながら、その計算結果が収まる`Schema`を同じ規則で組み立てていました。
`output_schema()`のためだけにこの規則をもう一度書くと、2つの実装が別々の場所で同じ判断(単純な列参照は列定義を引き継ぐ、計算結果は`data_type()`を使う)を下すことになり、どちらか一方だけ規則を変えてしまう事故が起こりえます。
この章では、この規則を`projection_schema`という1つの関数として独立させ、`output_schema()`と`executor::project`の両方がこの関数を呼ぶ形に揃えました。
列構成の決め方が2箇所に分かれてずれる余地は、これで無くなります。

## Bound ASTから演算子木を組み立てる

`BoundStatement`の各バリアントから`LogicalPlan`を組み立てる関数を、文の種類ごとに用意します。

`SELECT`は、`FROM`があれば`Scan`を、無ければ列を持たない行を1件生成する`Values`を根にします。
どちらの場合も、`WHERE`があれば`Filter`を挟み、最後に必ず`Projection`を積みます。

```rust
pub fn build_select(select: BoundSelect) -> LogicalPlan {
    let source = match select.tables.into_iter().next() {
        Some(table) => LogicalPlan::Scan(ScanNode {
            table_id: table.table_id,
            table_name: table.table_name,
            schema: table.schema,
        }),
        None => LogicalPlan::Values(ValuesNode {
            schema: Schema::new(Vec::new()),
            rows: vec![Vec::new()],
        }),
    };

    let filtered = match select.predicate {
        Some(predicate) => LogicalPlan::Filter(FilterNode { input: Box::new(source), predicate }),
        None => source,
    };

    LogicalPlan::Projection(ProjectionNode { input: Box::new(filtered), projection: select.projection })
}
```

`WHERE`が無いSQLに対して、この関数は`Filter`ノードをそもそも作りません。
`filtered`は`source`(`Scan`または`Values`)をそのまま指すだけで、木の形自体が「`Filter`を経由しない」ことを表します。
`SELECT id FROM users`のような`WHERE`の無い`SELECT`は、`Projection(id) └─ Scan(users)`という2段の木になり、`execute_select_with_from`が持っていた「`predicate`があるかどうかで処理を分岐する」という`if`文は、木を組み立てる`build_select`の中だけに残ります。

`INSERT`は、`VALUES`の各行を`Values`ノードへそのまま積み、それを子に持つ`Insert`を組み立てます。

```rust
pub fn build_insert(insert: BoundInsert) -> LogicalPlan {
    let values = LogicalPlan::Values(ValuesNode { schema: insert.schema.clone(), rows: insert.rows });
    LogicalPlan::Insert(InsertNode {
        table_id: insert.table_id,
        table_name: insert.table_name,
        schema: insert.schema,
        columns: insert.columns,
        input: Box::new(values),
    })
}
```

`BoundInsert`には、この章から`table_name`というフィールドを加えました。
第17章の`resolve_table`はテーブル名をすでに`BoundTableRef::table_name`として持っていましたが、`bind_insert`、`bind_update`、`bind_delete`はそれを使わずに捨てていました。
`LogicalPlan`の木を表示する際([次節](#演算子木を表示する)参照)にテーブル名を出すには、この情報が必要です。
すでに`resolve_table`が計算していた値を捨てずに運ぶだけの変更なので、名前解決のやり方自体は変わりません。

`UPDATE`、`DELETE`は、対象テーブルを表す`Scan`を子に持つ`Update`、`Delete`を組み立てます。

```rust
pub fn build_update(update: BoundUpdate) -> LogicalPlan {
    let scan = LogicalPlan::Scan(ScanNode {
        table_id: update.table_id,
        table_name: update.table_name.clone(),
        schema: update.schema.clone(),
    });
    LogicalPlan::Update(UpdateNode {
        table_id: update.table_id,
        table_name: update.table_name,
        schema: update.schema,
        assignments: update.assignments,
        predicate: update.predicate,
        input: Box::new(scan),
    })
}
```

`build_delete`も同じ形です。
`CREATE TABLE`、`DROP TABLE`には対応する`build_*`関数がありません。
`Binder`が`CREATE TABLE`を素通しした(第17章)のと同じ理由で、これらの文はまだ存在しないカタログエントリを定義または削除するだけの文であり、関係代数の演算子(既存の表に対する操作)として表現する対象がそもそも無いからです。

## 演算子木を表示する

`LogicalPlan`に`Display`を実装し、木をそのままの形で見られるようにします。

```rust
impl fmt::Display for LogicalPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_tree(f, 0)
    }
}
```

`write_tree`は、自分自身のラベル(`label()`)を1行書いてから、子(`children()`)それぞれに対して自分自身を再帰呼び出しする、素朴な木の描画です。
深さが増えるたびにインデントを2文字ずつ深くします。

```rust
fn write_tree(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
    if depth == 0 {
        writeln!(f, "{}", self.label())?;
    } else {
        let indent = "  ".repeat(depth);
        writeln!(f, "{indent}└─ {}", self.label())?;
    }
    for child in self.children() {
        child.write_tree(f, depth + 1)?;
    }
    Ok(())
}
```

`SELECT name FROM users WHERE id = 42`を`build_select`に通し、`to_string()`すると次の文字列になります。

```text
Projection(name)
  └─ Filter(id = 42)
    └─ Scan(users)
```

`Filter(id = 42)`の`id = 42`は、`filter.predicate`(`BoundExpr::BinaryOp`)を`fmt_bound_expr`という補助関数で読める形に変換したものです。
`fmt_bound_expr`は式の構造(演算子と被演算子)から文字列を組み立て直すだけで、元のSQL文字列の該当箇所(`Span`)は経由しません。
そのため`SELECT id+1`と空白を詰めて書いても、表示は`id + 1`のように揃った空白の入り方になります。
この表示はSQLへ逆変換する用途を意図したものではなく、木を人が読むためだけの表現です。

この`Display`実装は`EXPLAIN`そのものではありません。
実行アルゴリズムをまだ持たない`LogicalPlan`をそのまま覗き見るための表現であり、`Scan`が実際に全件走査になるか索引を使うかのような、`Physical Plan`が確定させる情報はここには含まれません。
利用者向けの`EXPLAIN`コマンドの実装は第19章に譲ります。

## `Database::execute`をparse→bind→plan→executeへ再編する

`src/database.rs`の`Database::execute`は、束縛の直後に計画を組み立てる1行が増えます。

```rust
pub fn execute(&mut self, sql: &str) -> DbResult<QueryResult> {
    let statement = crate::parser::parse_statement(sql)?;
    let bound = self.bind(statement, sql)?;
    match bound {
        BoundStatement::Select(select) => self.execute_select(logical_plan::build_select(select)),
        BoundStatement::CreateTable(create) => self.execute_create_table(&create),
        BoundStatement::DropTable(drop) => self.execute_drop_table(&drop),
        BoundStatement::Insert(insert) => self.execute_insert(logical_plan::build_insert(insert)),
        BoundStatement::Update(update) => self.execute_update(logical_plan::build_update(update)),
        BoundStatement::Delete(delete) => self.execute_delete(logical_plan::build_delete(delete)),
    }
}
```

`LogicalPlan`への変換自体は失敗しません。
`Binder`がすでに名前、型を確定させているため、`BoundStatement`から`LogicalPlan`への変換は形を組み替えるだけで、新たに検出すべき誤りが無いからです。
`CREATE TABLE`、`DROP TABLE`は`LogicalPlan`を経由せず、これまでどおりテーブル定義を直接登録または削除します。

`execute_select`は、`eval_query_plan`という1つの再帰関数へ木を渡すだけになりました。

```rust
fn execute_select(&self, plan: LogicalPlan) -> DbResult<QueryResult> {
    let (schema, rows) = self.eval_query_plan(&plan)?;
    Ok(QueryResult { schema, rows, command_tag: None })
}
```

`eval_query_plan`が、`LogicalPlan`の根から葉へたどりながら`executor`の演算子を適用する本体です。

```rust
fn eval_query_plan(&self, plan: &LogicalPlan) -> DbResult<(Schema, Vec<Tuple>)> {
    match plan {
        LogicalPlan::Scan(scan) => {
            let rows = match &self.backend {
                Backend::Memory { storage, .. } => { /* ... */ executor::seq_scan(mem_table) }
                Backend::Disk { storage } => executor::storage_seq_scan(storage, scan.table_id, &scan.schema)?,
            };
            Ok((scan.schema.clone(), rows))
        }
        LogicalPlan::Values(values) => {
            let mut rows = Vec::with_capacity(values.rows.len());
            for row_exprs in &values.rows {
                let evaluated = row_exprs
                    .iter()
                    .map(|expr| eval::eval_expr(expr, &self.functions, None))
                    .collect::<DbResult<Vec<_>>>()?;
                rows.push(Tuple::new(&values.schema, evaluated)?);
            }
            Ok((values.schema.clone(), rows))
        }
        LogicalPlan::Filter(filter) => {
            let (schema, rows) = self.eval_query_plan(&filter.input)?;
            let filtered = executor::filter(&schema, &self.functions, rows, &filter.predicate)?;
            Ok((schema, filtered))
        }
        LogicalPlan::Projection(projection) => {
            let (schema, rows) = self.eval_query_plan(&projection.input)?;
            executor::project(&schema, &self.functions, &rows, &projection.projection)
        }
        LogicalPlan::Insert(_) | LogicalPlan::Update(_) | LogicalPlan::Delete(_) => {
            unreachable!("Insert/Update/DeleteはSELECTの計画に現れない(logical_plan::build_selectは作らない)")
        }
    }
}
```

`Filter`、`Projection`の分岐は、自分自身を再帰呼び出ししてから`executor`の演算子を1回適用するだけです。
木の深さが増えても、この関数のコード自体は変わりません。

この再編で、第17章までの`execute_select_without_from`は丸ごと削除できました。
`FROM`を伴わない`SELECT`は`build_select`が`Values(1 row)`を根にすることで表現され、`eval_query_plan`はそれを`Scan`と同じ扱いで処理します。
`Values`の1行が`Filter`によって0件に絞られれば、後続の`Projection`はその0件に対してだけ動くため、`WHERE`が`TRUE`にならなかった暗黙の1行に対して射影式を評価してしまうこともありません。
これは`Filter`が先に行を絞り込んでから`Projection`が動くという、演算子の順序そのものが持つ性質です。
第17章までの`execute_select_without_from`は、この性質を「`matched`という真偽値を手で追跡し、`matched`が`false`なら射影式を評価しない」という条件分岐として自前で再現していましたが、`Filter`と`Projection`を独立した演算子として素直に合成するだけで、同じ性質が手続きを書かずに手に入ります。

`execute_insert`、`execute_update`、`execute_delete`は、`LogicalPlan`から必要な情報をパターンマッチで取り出してから、これまでと同じ`executor`の関数を呼びます。

```rust
fn execute_insert(&mut self, plan: LogicalPlan) -> DbResult<QueryResult> {
    let LogicalPlan::Insert(InsertNode { table_id, schema, columns, input, .. }) = plan else {
        unreachable!("logical_plan::build_insertは常にLogicalPlan::Insertを返す")
    };
    let LogicalPlan::Values(values) = *input else {
        unreachable!("logical_plan::build_insertはInsertの子に常にValuesを積む")
    };

    let count = match &mut self.backend {
        Backend::Memory { storage, .. } => {
            let mem_table =
                storage.table_mut(table_id).expect("catalogに登録されたテーブルはstorageにも必ず存在する");
            executor::insert(mem_table, &schema, &self.functions, columns.as_deref(), &values.rows)?
        }
        Backend::Disk { storage } => {
            executor::storage_insert(storage, table_id, &schema, &self.functions, columns.as_deref(), &values.rows)?
        }
    };
    Ok(QueryResult::command_with_count("INSERT", count))
}
```

`unreachable!`が2つ並んでいるのは、`plan`の型が`LogicalPlan`という7種類のバリアントを持つ`enum`である以上、コンパイラは「`execute_insert`に渡ってくるのは常に`Insert(Values)`の形をした木である」ことを知らないからです。
`execute_insert`は`Database::execute`から`logical_plan::build_insert`の戻り値だけを渡される、という呼び出し方の約束によってこの形を保証していますが、その約束はRustの型システムには現れません。
`Executor` traitで演算子を多態に扱う設計(第19章)に進めば、この`unreachable!`は要らなくなります。
この章の時点では、`Database`の内部だけで完結する呼び出し規約として残しています。

`execute_update`、`execute_delete`も同じ形で、`LogicalPlan::Update`、`LogicalPlan::Delete`から`table_id`、`schema`、`assignments`(または`predicate`)を取り出すだけです。
`UpdateNode::input`、`DeleteNode::input`(どちらも`Scan`)は取り出さず、実際にはたどりません。
`executor::update`、`executor::delete`自身が、走査と書き換え(または削除)を1回にまとめて行うためです。

## テストで確認する

`src/logical_plan.rs`には、各文種が正しい形の木になることを確認するテストを追加しました。
木の形の検証は、`to_string()`した結果を期待する文字列と比較する、ゴールデンテストに近いやり方です。

```rust
#[test]
fn select_with_from_and_where_builds_projection_over_filter_over_scan() {
    let catalog = users_catalog();
    let crate::binder::BoundStatement::Select(select) = bind("SELECT name FROM users WHERE id = 42", &catalog)
    else {
        panic!("Selectを期待した");
    };
    let plan = build_select(select);
    assert_eq!(
        plan.to_string(),
        "Projection(name)\n  └─ Filter(id = 42)\n    └─ Scan(users)\n"
    );
}
```

`WHERE`の無い`SELECT`が`Filter`ノードを作らないこと、`FROM`の無い`SELECT`が`Values`を根にすること、`output_schema()`が列参照の型、nullableを正しく引き継ぐことも、それぞれ個別のテストで確認しています。
`INSERT`、`UPDATE`、`DELETE`は、`Insert(Values)`、`Update(Scan)`、`Delete(Scan)`という木の形と、`Insert`、`Update`、`Delete`の`output_schema()`が空の`Schema`を返すことを確認しました。

`cargo test`を実行すると、`logical_plan`モジュールの新規テストを含め、`cargo test --lib`で344件の単体テスト、`differential`、`golden`、`persistence`の統合テストがすべて緑になります。

```console
$ cargo test
test result: ok. 344 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.55s
...
test golden_tests_pass ... ok
```

golden、differentialテストの期待値に変更はありません。
`Database::execute`の外から見える振る舞い(`QueryResult`の中身、エラーメッセージ)は、この章の再編の前後で変わっていないからです。

## 演習問題

### 必須課題

1. `LogicalPlan::Values`の`rows`が2件以上ある場合(`INSERT INTO t VALUES (1), (2), (3)`)、`label()`は`Values(3 rows)`のように件数だけを表示し、各行の中身は表示しません。`write_tree`を変更し、`Values`ノードの子として、行ごとの値を1行ずつ表示するようにしてください(`Values`はまだ子を持たない葉として設計してあるので、`children()`をどう変えるべきか、あるいは`label()`側で複数行を組み立てるべきかを、`LogicalPlan`が今持っている「ノード1個につき1行」という前提と照らし合わせながら考えてください)。
2. `Update`、`Delete`は、`predicate`を`UpdateNode`、`DeleteNode`自身のフィールドとして持ちつつ、子の`Scan`とは独立させてあります。この設計を変更し、`predicate`がある場合は`Update`、`Delete`の子を`Scan`ではなく`Filter(Scan)`にする(`Filter`ノードを間に挟む)案を実装し、`build_update`、`build_delete`、`write_tree`の表示、`execute_update`、`execute_delete`のそれぞれで何を変える必要があるか確認してください。この変更によって`executor::update`、`executor::delete`の実装(1回の走査にまとめる方式)まで変える必要があるかどうかも考えてください。
3. `fmt_bound_expr`は、`BinaryOp`の両辺に必ず空白を1つずつ入れて表示します(`id = 42`)。`BoundExpr::Paren`をどう表示するかによって、`(id + 1) * 2`のような式の優先順位が表示上も正しく伝わるかどうかが変わります。`Paren`を持たない`1 + 2 * 3`(`Parser`が優先順位に従って`BinaryOp(Add, 1, BinaryOp(Multiply, 2, 3))`という木に組み立てる式)を`fmt_bound_expr`に通すとどう表示されるか確認し、必要であれば演算子の優先順位を見て括弧を補う版を実装してください。

### 発展課題

1. `LogicalPlan::output_schema()`は呼び出しのたびに`Schema`を複製して返します。`Projection`が根にある木では、`output_schema()`を1回呼ぶたびに子の`output_schema()`も再帰的に呼ばれ、木の深さの分だけ`Schema`の複製が積み重なります。この章の`LogicalPlan`はまだ`Filter`と`Projection`だけの浅い木なのでコストは問題になりませんが、第22章で`Join`が加わり木が深くなったときにこの再帰コストがどう影響するかを考え、`output_schema`をキャッシュする設計(各ノードが自分の`output_schema`を構築時に1度だけ計算して保持する、など)を検討してください。
2. `logical_plan::build_select`は`BoundSelect`を消費(`self`ではなく値渡し)して`LogicalPlan`を組み立てます。この設計を、`&BoundSelect`を借用する形に変更しようとすると、`LogicalPlan`の各ノードが持つフィールド(`Schema`、`BoundExpr`、`Vec<BoundSelectItem>`など)をすべて借用に変える必要があります。実際に型を変更してコンパイラのエラーを確認し、`BoundStatement`を1度実行するだけのこのクレートの用途において、値渡し(消費)と借用のどちらが素直かを考えてください。
