# 第19章 Physical PlanとVolcano Executor

`SELECT name FROM users WHERE id = 42`を10万行のテーブルに対して実行したとき、`id = 42`に一致する行がどれだけ少なくても、実行中のメモリ使用量は変わらないでしょうか。

答えは、前章までのコードでは「変わる」です。
一致する行が1行だけでも、`WHERE`を評価する前に10万行分の`Tuple`が一度メモリに載ります。
この章では、その理由をコードで確かめてから、載らないように実行方式そのものを作り直します。

## 前章の限界

第18章の`Database::eval_query_plan`は、`LogicalPlan`の木を根から葉へたどりながら`executor`モジュールの演算子を呼び出す再帰関数でした。

```rust
fn eval_query_plan(&self, plan: &LogicalPlan) -> DbResult<(Schema, Vec<Tuple>)> {
    match plan {
        LogicalPlan::Scan(scan) => {
            let rows = /* ... テーブル全体を読む ... */;
            Ok((scan.schema.clone(), rows))
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
        // ...
    }
}
```

`Scan`は`(Schema, Vec<Tuple>)`というペアを返します。
`Filter`は子の`eval_query_plan`を呼んで`Vec<Tuple>`をまるごと受け取り、`executor::filter`に渡して絞り込んだ新しい`Vec<Tuple>`をまるごと作ります。
`Projection`も同じ形で、子の結果をまるごと受け取ってから新しい`Vec<Tuple>`をまるごと作ります。

「まるごと」という言葉を3回使ったのは、誇張ではありません。
`executor::filter`の中身を思い出すと、これがそのまま実装になっていたことが分かります。

```rust
pub fn filter(
    schema: &Schema,
    functions: &FunctionRegistry,
    rows: Vec<Tuple>,
    predicate: &BoundExpr,
) -> DbResult<Vec<Tuple>> {
    let mut kept = Vec::with_capacity(rows.len());
    for tuple in rows {
        // ... 一致した行だけをkeptへ積む ...
    }
    Ok(kept)
}
```

`rows`という引数そのものが、子の`Scan`(または別の`Filter`)がすでに作り終えた`Vec<Tuple>`です。
`filter`はこの`Vec`を先頭から最後まで見終えてからでなければ、呼び出し元へ何も返せません。
`WHERE id = 42`が10万行のうち1行にしか一致しなくても、`kept`という新しい`Vec`を作り切るまでの間、`rows`という10万行分の`Vec`は解放されずにメモリへ残り続けます。

`users`に10万行挿入し、`id`の最大値だけを一致させる`SELECT`を実行する場合を、コードをたどって数えてみます。
`Scan`、`Filter`、`Projection`はどれも決まった手順しか踏まないので、`eval_query_plan`が経由する`Vec`の要素数は次のように決まります。

```text
Scanが返すVec<Tuple>:      100,000要素
Filterに渡されるrows:       100,000要素(=Scanの結果を丸ごと受け取る)
Filterがkeptに積む要素数:        1要素
Filterが返すVec<Tuple>:          1要素
Projectionに渡されるrows:        1要素
```

`Scan`から`Filter`へ渡る`rows`だけが常にテーブル全体のサイズに比例します。
`Filter`が返した後は1要素まで絞られますが、そこに至るまでの一瞬、`rows`という10万要素の`Vec`と`kept`という1要素の`Vec`が同時にメモリ上に存在します。
テーブルがさらに大きくなれば、この一瞬に必要なメモリも比例して増え続けます。
`WHERE`が絞り込む行数とは無関係に、中間結果のピークサイズは常に「その`Scan`が読んだ行数」で決まってしまうのです。

これは`eval_query_plan`という関数の書き方が持つ性質であり、`LogicalPlan`という木そのものの性質ではありません。
`LogicalPlan`は「`Filter`の後に`Projection`が続く」という順序しか決めておらず、その順序をどう実行するか(段ごとに`Vec`を作り直すか、1行ずつ流すか)は決めていません(第18章の`Update`、`Delete`のドキュメントコメントが「実行方法の選択は第19章のPhysical Planが決める領域」と書いていたのは、まさにこの選択のことです)。
この章では、「段ごとに`Vec`を作り直す」実行方式を、「1行ずつ流す」実行方式に置き換えます。

## Volcanoモデル: `next()`は根から葉へ、タプルは葉から根へ

行を1件ずつ流す実行方式を、**Volcanoモデル**と呼びます。
[第2章](./ch02-life-of-a-query.md)で経路の概観として触れたとおり、各演算子は共通のインターフェースを実装します。

```rust
trait Executor {
    fn next(&mut self) -> Result<Option<Tuple>>;
}
```

`next()`は呼ばれるたびに、その演算子が生成する行をちょうど1件返します。
生成できる行が尽きれば`None`を返します。
この章で実際に定義する`Executor` traitは、`Result`の中身をこのクレートの`DbResult`に合わせ、`output_schema`というメソッドをもう1つ持ちます(定義は後の節で示します)。
`output_schema`が要るのは、`SELECT name FROM users`の結果が`TEXT`型の`name`列1つであることを、行を1件も引く前に呼び出し側(`QueryResult`を組み立てる`Database::execute_select`)が知る必要があるからです。

呼び出しの向きと、行が流れる向きは逆です。
`Projection(name) └─ Filter(id = 42) └─ SeqScan(users)`という木で`Projection::next()`が呼ばれると、`Projection`はまず子である`Filter`の`next()`を呼びます。
`Filter`は自分の子である`SeqScan`の`next()`を、条件に一致する行が見つかるまで繰り返し呼びます。
`SeqScan`はStorage Engineから1行取り出して返し、`Filter`はその行が`id = 42`に一致するかどうかを確かめ、一致すればそのまま`Filter::next()`の戻り値として返します。
`Projection`はその1行を受け取り、`name`列だけを取り出して自分の呼び出し元へ返します。

「呼び出しは根から葉へ下り、行は葉から根へ返る」という向きは、前章までの`eval_query_plan`とは違う点が1つあります。
`eval_query_plan`は、子の呼び出しが**完全に終わってから**(`Vec`を作り終えてから)親の処理に移っていました。
Volcanoモデルの`next()`は、子の`next()`を1回呼んで1行受け取るたびに、親がすぐその1行を加工して返します。
`Filter`が条件に一致しない行を読み飛ばす間も、`kept`のような`Vec`へ何かを積むことはありません。
その場で捨てて、次の子の行へ進むだけです。

## `PhysicalPlan`: 実行アルゴリズムを確定した木

`LogicalPlan`の`Scan`は、「テーブルを走査する」ことしか表していませんでした。
索引がまだ無いこの章のクレートでは、走査は常に全件走査(Sequential Scan)にしかなりようがありませんが、それでも「走査する」という意図と「全件走査というアルゴリズムを使う」という決定は別の階層の話です。
第25章でB+Tree索引を使ったIndex Scanが加わったとき、`Scan`は状況に応じてSequential ScanとIndex Scanのどちらかへ変換されるようになります。
その変換の受け皿を、この章のうちに用意しておきます。

`LogicalPlan`とほぼ同じ形で、実行アルゴリズムを確定した木を`PhysicalPlan`という別の型として定義します。

```rust
pub enum PhysicalPlan {
    SeqScan(SeqScanNode),
    Values(ValuesNode),
    Filter(FilterNode),
    Projection(ProjectionNode),
    Insert(InsertNode),
    Update(UpdateNode),
    Delete(DeleteNode),
}
```

`Scan`が`SeqScan`という具体的な名前に変わった以外、`LogicalPlan`とバリアントの構成は同じです。
`LogicalPlan`から`PhysicalPlan`への変換は、`optimize`という1つの関数が担います。

```rust
pub fn optimize(plan: LogicalPlan) -> PhysicalPlan {
    match plan {
        LogicalPlan::Scan(scan) => PhysicalPlan::SeqScan(SeqScanNode {
            table_id: scan.table_id,
            table_name: scan.table_name,
            schema: scan.schema,
        }),
        LogicalPlan::Filter(filter) => {
            PhysicalPlan::Filter(FilterNode { input: Box::new(optimize(*filter.input)), predicate: filter.predicate })
        }
        // Values・Projection・Insert・Update・Deleteも同様に子を再帰的にoptimizeする
        // ...
    }
}
```

この章の時点で`optimize`が行っているのは、`Scan`を`SeqScan`という名前に変えるだけの、選びようのない変換です。
それでも関数名を「変換する」ではなく`optimize`にしたのは、この関数が将来担う役割を先取りしているからです。
第25章で`optimize`は、`id`列に索引があるかどうかや、統計情報から推定した一致行数(第27章)をもとに、`Scan`を`SeqScan`と`IndexScan`のどちらへ変換するかを本当に選ぶようになります。
この章の実装は、その選択肢がまだ1つしか無い特殊ケースにすぎません。

`PhysicalPlan`は`output_schema()`、`Display`実装のどちらも`LogicalPlan`と同じ考え方で持ちます。
`output_schema()`は`LogicalPlan::output_schema`と全く同じ規則に従い、`Projection`の列構成は同じ`logical_plan::projection_schema`関数を呼んで決めます(決め方を2箇所に分けないという第18章の方針をここでも踏襲します)。
`Display`は`LogicalPlan`と同じ木の描画ロジックを使い、`SELECT name FROM users WHERE id = 42`を`optimize`に通すと次の文字列になります。

```text
Projection(name)
  └─ Filter(id = 42)
    └─ SeqScan(users)
```

`LogicalPlan`の`Display`(第18章)との違いは`Scan`が`SeqScan`という具体的なアルゴリズム名で表示される点だけです。
この表示は、次に実装する`EXPLAIN`がそのまま利用者へ返す文字列になります。

## `Executor` trait: Trait Object方式を選ぶ

`Executor`をどう実装するかには、大きく2つの方式があります。

1つは、この章で採用する**Trait Object方式**です。
`FilterExec`、`ProjectionExec`のように演算子ごとに異なる`struct`を定義し、それぞれに`impl Executor`を書きます。
親の演算子は、子を`Box<dyn Executor>`という共通の型で持ちます。

もう1つは**Enum Dispatch方式**です。
`PhysicalPlan`のように演算子の種類を1つの`enum`で表し、`next()`の中身を`match`で分岐させます。
`Executor`という trait そのものを作らず、`PhysicalPlan`の`match`アームに実行ロジックを直接書き込む形になります。

2つの方式には、それぞれ向き不向きがあります。
Trait Object方式は、`Box<dyn Executor>`を介した`next()`の呼び出しのたびに、実際の型(`FilterExec`なのか`ProjectionExec`なのか)をvtable経由で調べる動的ディスパッチのコストがかかります。
Enum Dispatch方式は、コンパイル時に`match`の分岐先が確定するため、この動的ディスパッチが要りません。
一方で、演算子の種類が増えるたびに、Enum Dispatch方式は1つの巨大な`match`式(`next()`の実装全体)へ分岐を書き足していく形になります。
Trait Object方式であれば、演算子ごとに独立した`struct`と`impl Executor`を追加するだけで済み、既存の演算子の実装には触れません。

この章ではTrait Object方式を選びました。
今後の章(第21章の`Sort`、`Limit`、`Distinct`、`Aggregate`、第22章の`Join`、第25章の`IndexScan`)で演算子の種類を継続的に増やしていくこのクレートの育て方には、演算子ごとに実装を閉じ込められるTrait Object方式のほうが向いています。
動的ディスパッチのコストが実際にどれだけ効くかは、この章では測定しません(章末の演習課題で、Enum Dispatch方式を実装して比較します)。

```rust
pub trait Executor {
    fn output_schema(&self) -> &Schema;
    fn next(&mut self) -> DbResult<Option<Tuple>>;
}
```

## 演算子を実装する

`SeqScan`、`Values`、`Filter`、`Projection`の4つを、`Executor`を実装する`struct`として書き直します。

### Values: 構築時にまとめて評価してよい理由

`ValuesExec`は`VALUES`の各行を、構築時にまとめて評価します。

```rust
pub struct ValuesExec {
    schema: Schema,
    rows: std::vec::IntoIter<Tuple>,
}

impl ValuesExec {
    pub fn new(schema: Schema, functions: &FunctionRegistry, row_exprs: &[Vec<Expr>]) -> DbResult<Self> {
        let mut rows = Vec::with_capacity(row_exprs.len());
        for exprs in row_exprs {
            let values = exprs.iter().map(|expr| eval_expr(expr, functions, None)).collect::<DbResult<Vec<_>>>()?;
            rows.push(Tuple::new(&schema, values)?);
        }
        Ok(ValuesExec { schema, rows: rows.into_iter() })
    }
}

impl Executor for ValuesExec {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        Ok(self.rows.next())
    }
}
```

`next()`が呼ばれるたびに1行ずつ評価する形にもできますが、そうしませんでした。
`VALUES (1, 'a'), (2, 'b')`という行数は、SQL文の長さそのものに比例します。
テーブルの行数のように無制限に増える値ではなく、せいぜい数十行から数百行です。
構築時に全行を評価しても、この章がストリーミング実行で守ろうとしている性質(中間結果のサイズがテーブル全体に比例しない)を損ないません。

### SeqScan: `MemTable`版と`Storage`版

`SeqScan`は、第16章から続く2つの供給源(`MemTable`と`Storage`)に対応する2つの`struct`を持ちます。

```rust
pub struct MemSeqScanExec<'a> {
    schema: &'a Schema,
    rows: std::slice::Iter<'a, Tuple>,
}

impl<'a> MemSeqScanExec<'a> {
    pub fn new(schema: &'a Schema, table: &'a MemTable) -> Self {
        MemSeqScanExec { schema, rows: table.rows().iter() }
    }
}

impl<'a> Executor for MemSeqScanExec<'a> {
    fn output_schema(&self) -> &Schema {
        self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        Ok(self.rows.next().cloned())
    }
}
```

第18章までの`executor::seq_scan`は`table.rows().to_vec()`でテーブル全体を複製していました。
`MemSeqScanExec`は`std::slice::Iter`を1歩ずつ進めるだけなので、`next()`が呼ばれた分しか複製が起きません。

`Storage`版は、`Storage::scan`(第15章)が返す`Scan`イテレータをそのまま持ちます。

```rust
pub struct DiskSeqScanExec<'a> {
    schema: &'a Schema,
    scan: HeapScan<'a>,
}

impl<'a> DiskSeqScanExec<'a> {
    pub fn new(storage: &'a Storage, table_id: TableId, schema: &'a Schema) -> DbResult<Self> {
        Ok(DiskSeqScanExec { schema, scan: storage.scan(table_id)? })
    }
}

impl<'a> Executor for DiskSeqScanExec<'a> {
    fn output_schema(&self) -> &Schema {
        self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        match self.scan.next() {
            None => Ok(None),
            Some(entry) => {
                let (_, bytes) = entry?;
                decode_tuple(self.schema, &bytes).map(Some)
            }
        }
    }
}
```

`Storage::scan`が返す`Scan`は、もともとページを1枚ずつ`BufferPool`から取り出す遅延評価のイテレータでした(第14、15章)。
第18章までの`executor::storage_seq_scan`は、このイテレータを`collect()`して`Vec<Tuple>`へまとめていました。
`DiskSeqScanExec`はこの`collect()`という一手間を無くし、イテレータをそのまま1件ずつ`decode_tuple`へ通します。

### Filter: 一致するまで子を引き、一致しない行は溜めない

```rust
pub struct FilterExec<'a> {
    input: Box<dyn Executor + 'a>,
    predicate: &'a BoundExpr,
    functions: &'a FunctionRegistry,
    schema: Schema,
}

impl<'a> FilterExec<'a> {
    pub fn new(input: Box<dyn Executor + 'a>, predicate: &'a BoundExpr, functions: &'a FunctionRegistry) -> Self {
        let schema = input.output_schema().clone();
        FilterExec { input, predicate, functions, schema }
    }
}

impl<'a> Executor for FilterExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        loop {
            let Some(tuple) = self.input.next()? else {
                return Ok(None);
            };
            let row = Row::new(&self.schema, &tuple);
            let value = eval_bound_expr(self.predicate, self.functions, Some(&row))?;
            if predicate_matches(value)? {
                return Ok(Some(tuple));
            }
        }
    }
}
```

`next()`1回の呼び出しは、子の`next()`を「条件に一致する行が見つかるまで」の回数だけ呼びます。
一致しなかった行は`row`という変数に一瞬だけ現れ、`predicate_matches`が`false`を返した時点で`loop`が次の反復に進み、その行への参照は破棄されます。
`kept`のような`Vec`は、`FilterExec`のどこにも存在しません。

`predicate_matches`は第10章から続く関数で、`WHERE`の評価結果を三値論理に従って`bool`へ変換します(`TRUE`だけがマッチし、`FALSE`と`NULL`はどちらもマッチしない)。
この章で`executor`モジュールに残し、`pub(crate)`にして`physical_plan`モジュールからも呼べるようにしました。

### Projection: 1行受け取り、1行返す

```rust
pub struct ProjectionExec<'a> {
    input: Box<dyn Executor + 'a>,
    projection: &'a [BoundSelectItem],
    functions: &'a FunctionRegistry,
    input_schema: Schema,
    out_schema: Schema,
}

impl<'a> ProjectionExec<'a> {
    pub fn new(input: Box<dyn Executor + 'a>, projection: &'a [BoundSelectItem], functions: &'a FunctionRegistry) -> Self {
        let input_schema = input.output_schema().clone();
        let out_schema = logical_plan::projection_schema(&input_schema, projection);
        ProjectionExec { input, projection, functions, input_schema, out_schema }
    }
}

impl<'a> Executor for ProjectionExec<'a> {
    fn output_schema(&self) -> &Schema {
        &self.out_schema
    }

    fn next(&mut self) -> DbResult<Option<Tuple>> {
        let Some(tuple) = self.input.next()? else {
            return Ok(None);
        };
        let row = Row::new(&self.input_schema, &tuple);
        let mut values = Vec::with_capacity(self.projection.len());
        for item in self.projection {
            values.push(eval_bound_expr(&item.expr, self.functions, Some(&row))?);
        }
        Tuple::new(&self.out_schema, values).map(Some)
    }
}
```

`next()`1回につき、子から1行受け取り、`projection`を適用した1行を返して終わりです。
`out_rows`のような`Vec`を組み立てる段階自体がありません。

### 組み立て: `Database::build_query_executor`

`PhysicalPlan`の木から`Box<dyn Executor>`の入れ子を組み立てるのは、`Database`のprivateメソッドです。

```rust
fn build_query_executor<'a>(&'a self, plan: &'a PhysicalPlan) -> DbResult<Box<dyn Executor + 'a>> {
    match plan {
        PhysicalPlan::SeqScan(scan) => {
            let exec: Box<dyn Executor + 'a> = match &self.backend {
                Backend::Memory { storage, .. } => {
                    let mem_table = storage
                        .table(scan.table_id)
                        .expect("catalogに登録されたテーブルはstorageにも必ず存在する");
                    Box::new(MemSeqScanExec::new(&scan.schema, mem_table))
                }
                Backend::Disk { storage } => Box::new(DiskSeqScanExec::new(storage, scan.table_id, &scan.schema)?),
            };
            Ok(exec)
        }
        PhysicalPlan::Values(values) => {
            let exec = ValuesExec::new(values.schema.clone(), &self.functions, &values.rows)?;
            Ok(Box::new(exec))
        }
        PhysicalPlan::Filter(filter) => {
            let input = self.build_query_executor(&filter.input)?;
            Ok(Box::new(FilterExec::new(input, &filter.predicate, &self.functions)))
        }
        PhysicalPlan::Projection(projection) => {
            let input = self.build_query_executor(&projection.input)?;
            Ok(Box::new(ProjectionExec::new(input, &projection.projection, &self.functions)))
        }
        PhysicalPlan::Insert(_) | PhysicalPlan::Update(_) | PhysicalPlan::Delete(_) => {
            unreachable!("Insert/Update/DeleteはSELECTの計画に現れない(logical_plan::build_selectは作らない)")
        }
    }
}
```

`SeqScan`だけが`&self.backend`を見ます。
`Filter`、`Projection`は供給源を意識せず、`Box<dyn Executor>`という共通のインターフェースだけを相手にします。
`execute_select`は、この関数が組み立てた木の根に対して`next()`を呼び続けるだけになりました。

```rust
fn execute_select(&self, plan: LogicalPlan) -> DbResult<QueryResult> {
    let physical = physical_plan::optimize(plan);
    let schema = physical.output_schema();
    let mut executor = self.build_query_executor(&physical)?;

    let mut rows = Vec::new();
    while let Some(tuple) = executor.next()? {
        rows.push(tuple);
    }
    Ok(QueryResult { schema, rows, command_tag: None })
}
```

`rows`という1つの`Vec`に最終結果を集めているのは、`QueryResult`が`rows()`で`&[Tuple]`を返す型だからです。
この`Vec`自体は「最終的にクライアントへ返す結果の件数」に比例しますが、それは`SELECT`の性質上避けられません(利用者が最終的に見る結果は、どこかで1つの値として確定する必要があります)。
ストリーミング実行が効くのは、あくまで計画の**中間段階**(`Filter`を通過する前の候補行、`WHERE`に一致しなかった行)がメモリに残らないという点です。

## `INSERT`、`UPDATE`、`DELETE`は`Executor`にしない

`PhysicalPlan::Insert`、`Update`、`Delete`は木の一部として存在し、`EXPLAIN`の出力にもノードとして現れます。
しかし`SeqScan`、`Filter`、`Projection`と違い、これらを実行するのは`Executor`ではありません。
`Database::execute_insert`等は、第18章までと同じ`executor::insert`、`storage_insert`等の一括関数をそのまま呼びます。

Volcanoの子として`INSERT`、`UPDATE`、`DELETE`を分解しなかった理由は2つあります。

1点目は、**全件検査してから全件書き込む**という不変条件です。
`INSERT`の3行目が`NOT NULL`制約に違反していたら、1、2行目がすでに検査を通っていても1行も書き込まない、という「全部か無か」の性質は第10章から一貫しています。
`next()`が1行ごとに即座に書き込む設計にすると、この性質を保つには「書き込む前に全部バッファする」という段階を`Executor`の外にもう1つ用意する必要があり、Pull型実行の利点(段階を1つに減らす)を打ち消してしまいます。

2点目は、**書き込みには`&mut`の排他アクセスが要る**という、Rustの借用規則そのものに根差した理由です。
`UPDATE`、`DELETE`がVolcanoの子として`SeqScan`を持つ設計にすると、`UpdateExec`は「読み取り用に子`Executor`が持つ`&Storage`」と「書き込み用の`&mut Storage`」を同時に必要とすることになります。
子`Executor`は`next()`を呼び終えるまで`&Storage`を手放さないため、この2つを1つの構造体に共存させることはできません。
`executor::storage_update`(第16章)はすでに「`scan`で全件読み切ってから`update`で書き込む」という2段階の関数として実装済みであり、この章で新たに書き直す理由がありません。

`SELECT`(読み取りのみ)はこの制約を持ちません。
行を書き換えないので`&Backend`という共有参照だけで木全体を組み立てられ、`Filter`、`Projection`は純粋に「子から1行受け取って加工する」だけの演算子になります。
この非対称性(読み取りは真にストリーミング、書き込みは検証してから一括反映)が、この章の設計判断です。

## `EXPLAIN`を実装する

`EXPLAIN <SELECT|INSERT INTO|UPDATE|DELETE FROM>`という構文を、Lexer、Parser、Binderへ順に追加します。

Lexerには`EXPLAIN`という予約語を1つ追加します(`Keyword::Explain`)。
Parserは`EXPLAIN`の直後に、`SELECT`、`INSERT INTO`、`UPDATE`、`DELETE FROM`のいずれかだけを許します。

```rust
fn parse_explain_statement(&mut self) -> DbResult<ExplainStatement> {
    let start = self.expect_keyword(Keyword::Explain, "EXPLAIN")?.start;

    let statement = match self.peek_kind() {
        TokenKind::Keyword(Keyword::Select) => self.parse_select_statement().map(Statement::Select)?,
        TokenKind::Keyword(Keyword::Insert) => self.parse_insert_statement().map(Statement::Insert)?,
        TokenKind::Keyword(Keyword::Update) => self.parse_update_statement().map(Statement::Update)?,
        TokenKind::Keyword(Keyword::Delete) => self.parse_delete_statement().map(Statement::Delete)?,
        _ => return Err(self.unexpected("SELECT・INSERT INTO・UPDATE・DELETE FROMのいずれか")),
    };

    let end = statement.span().end;
    Ok(ExplainStatement { statement: Box::new(statement), span: Span { start, end } })
}
```

`CREATE TABLE`、`DROP TABLE`を対象から外したのは、この2つがどちらの計画も経由しない文だからです(`Binder`を素通りする理由は第17章、`LogicalPlan`を経由しない理由は第18章を参照)。
対象を`SELECT`等4種の解析関数だけに絞ったことで、`EXPLAIN EXPLAIN ...`のような入れ子も、生の`parse_statement`を再帰的に呼ばないこの書き方によって構文の時点で拒否されます。

`Binder`は対象の文をそのまま束縛するだけです。

```rust
Statement::Explain(explain) => {
    self.bind(*explain.statement).map(|inner| BoundStatement::Explain(Box::new(inner)))
}
```

`Database::execute_explain`は、束縛済みの文を`LogicalPlan`、`PhysicalPlan`へ変換し、木を文字列化しただけの`QueryResult`を返します。
実際には何も実行しません。

```rust
fn execute_explain(&self, inner: BoundStatement) -> DbResult<QueryResult> {
    let logical = match inner {
        BoundStatement::Select(select) => logical_plan::build_select(select),
        BoundStatement::Insert(insert) => logical_plan::build_insert(insert),
        BoundStatement::Update(update) => logical_plan::build_update(update),
        BoundStatement::Delete(delete) => logical_plan::build_delete(delete),
        BoundStatement::CreateTable(_) | BoundStatement::DropTable(_) | BoundStatement::Explain(_) => {
            unreachable!("ParserがEXPLAINの対象をSELECT・INSERT INTO・UPDATE・DELETE FROMに制限している")
        }
    };
    let physical = physical_plan::optimize(logical);
    Ok(QueryResult::explain(physical.to_string()))
}
```

`QueryResult::explain`は、PostgreSQLの`EXPLAIN`にならい、`QUERY PLAN`という1列の結果として木を返します。
木の1行が結果の1行になります。

```rust
fn explain(plan_text: String) -> Self {
    let schema = Schema::new(vec![Column::new("QUERY PLAN", DataType::Text, false)]);
    let rows = plan_text
        .lines()
        .map(|line| {
            Tuple::new(&schema, vec![Value::Text(line.to_string())])
                .expect("QUERY PLAN列はTEXTなので必ず成功する")
        })
        .collect();
    QueryResult { schema, rows, command_tag: None }
}
```

`EXPLAIN SELECT name FROM users WHERE id = 42`を実行すると、次の3行が返ります。

```text
QUERY PLAN
----------
Projection(name)
  └─ Filter(id = 42)
    └─ SeqScan(users)
(3 rows)
```

`EXPLAIN ANALYZE`(推定値と実測値の比較)は、統計情報とコストモデルが揃う第27章まで持ち越します。
この章の`EXPLAIN`は、実行アルゴリズムの木を見せるだけの、実測を伴わない簡易版です。

## テストで確認する

各演算子の`next()`が実際に1行ずつ流れることは、`physical_plan`モジュールに手作りの`CountingExecutor`(`next()`が呼ばれた回数を数える、テスト専用の葉演算子)を使って確認します。

```rust
#[test]
fn scan_filter_projection_pipeline_pulls_exactly_as_many_rows_as_requested() {
    // Scan相当のCountingExecutor→Filter→Projectionという3段の合成でも、
    // 根から3回`next()`を呼んだだけなら、葉は3回しか`next()`されない。
    let rows: Vec<Tuple> = (0..1000).map(|i| tuple(i, "x")).collect();
    let pulled = Rc::new(Cell::new(0));
    let leaf = CountingExecutor { schema: users_schema(), rows: rows.into_iter(), pulled: pulled.clone() };

    let predicate = bound_true_predicate();
    let projection = bound_id_projection();
    let functions = FunctionRegistry::with_builtins();
    let filter = FilterExec::new(Box::new(leaf), &predicate, &functions);
    let mut projection_exec = ProjectionExec::new(Box::new(filter), &projection, &functions);

    for _ in 0..3 {
        projection_exec.next().unwrap();
    }

    assert_eq!(pulled.get(), 3);
}
```

1,000行すべてが条件に一致する状況でも、根から3回しか`next()`を呼ばなければ、葉も3回しか`next()`されません。
これが第18章までの`eval_query_plan`(`Filter`が呼ばれた時点で1,000行すべてを読み切り、`Vec`にまとめてしまう)との違いです。

`database`モジュールには、より大きな規模でこの性質を確認する統合テストを追加しました。

```rust
#[test]
fn select_streams_rows_without_materializing_the_whole_table_at_once() {
    // 10,000行のテーブルに対し、最後の1行だけに一致する`WHERE`を実行する。
    // 第18章までの`eval_query_plan`なら、`Filter`が返す前の中間結果として
    // 10,000行分の`Tuple`を1つの`Vec`にまとめて保持していた。この章の
    // `next()`ループでは、`Filter`・`Projection`のどちらも子から1行ずつ
    // 引いて1行ずつ返すため、最終結果(1行)より大きな`Vec`はどの段階にも
    // 生まれない(この性質そのものは`physical_plan`モジュールの
    // `CountingExecutor`を使ったテストで、子が実際に何回`next()`されたかを
    // 数えて確認している。ここではその上で、10,000行規模でも結果が正しい
    // ことをend-to-endに確認する)。
    const N: i64 = 10_000;
    let mut db = users_db();
    let values: Vec<String> = (1..=N).map(|i| format!("({i}, 'user{i}')")).collect();
    db.execute(&format!("INSERT INTO users VALUES {}", values.join(", "))).unwrap();

    let result = db.execute(&format!("SELECT name FROM users WHERE id = {N}")).unwrap();
    assert_eq!(result.rows().len(), 1);
    assert_eq!(result.rows()[0].values(), &[Value::Text(format!("user{N}"))]);
}
```

このテストが確認しているのは実行結果の正しさ(1行だけが返ること)であり、メモリ使用量そのものではありません。
中間結果が全件バッファされないという性質自体は、`CountingExecutor`を使ったテストのように、子の`next()`が呼ばれた回数を数えることでしか観測できません(ヒープ使用量を計測するテストは環境に依存しやすく、この章では採用しません)。
永続モード(`Backend::Disk`)についても、同じ形の統合テストを追加し、両方のバックエンドで結果が一致することを確認しています。

`EXPLAIN`の出力は、`Projection(name)`、`Filter(id = 42)`、`SeqScan(users)`のようなゴールデンテスト(期待する文字列との完全一致)で確認します。
`cargo test`を実行すると、`physical_plan`モジュールの新規テストを含め、`cargo test --lib`で361件の単体テストと、`differential`、`golden`、`persistence`の統合テストがすべて緑になります。

```console
$ cargo test
test result: ok. 361 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.56s
...
test golden_tests_pass ... ok
```

golden、differentialテストの期待値に変更はありません。
`SELECT`、`INSERT`、`UPDATE`、`DELETE`の実行結果(`QueryResult`の中身、エラーメッセージ)は、実行方式を書き換えたこの章の前後で変わっていないからです。

## 演習問題

### 必須課題

1. `ProjectionExec::next()`は、子から1行受け取るたびに`values`という`Vec`を新しく確保します(`Vec::with_capacity(self.projection.len())`)。この確保を`ProjectionExec`のフィールドとして持つ再利用可能な`Vec`に置き換え、`next()`を呼ぶたびに`clear()`してから使い回す実装を試してください。ベンチマーク(`cargo bench`はまだこのクレートに無いので、簡単な`std::time::Instant`計測で構いません)で、確保回数を減らした効果がどれだけ見えるか確認してください。
2. `Executor` traitをTrait Object方式ではなくEnum Dispatch方式で実装し直してください。`PhysicalPlan`自身に`next(&mut self, state: &mut ExecutorState) -> DbResult<Option<Tuple>>`のようなメソッドを持たせ、`match`で分岐する形になります(`Filter`、`Projection`のように子を持つノードは、実行に必要な可変状態(`Values`の残り行など)をどう`PhysicalPlan`とは別に持たせるかが設計の要点です)。実装できたら、同じSQLをTrait Object版とEnum Dispatch版の両方で1,000,000回実行し、実行時間を比較してください。
3. `DiskSeqScanExec`は`Storage::scan`が返す`Scan`イテレータをそのまま使っていますが、`MemSeqScanExec`は`std::slice::Iter`を使っています。この2つに共通の`trait TupleSource { fn next_tuple(&mut self) -> DbResult<Option<(Schema参照や生バイト列など)>>; }`のような抽象を導入する案を検討し、実際にコンパイルが通るところまで実装したうえで、`executor`モジュール冒頭のコメントが述べていた「`MemTable`は`Vec`の添字、`Storage`は`RecordId`で指す」という非対称性がこの抽象化にどう影響するか考えてください。

### 発展課題

1. `FilterExec::next()`は、条件に一致しない行を読み飛ばす`loop`の中で、`Row::new`と`eval_bound_expr`を毎回呼び直しています。`WHERE id = 42`のように述語の右辺が定数の場合、この定数の評価(`42`という`IntLiteral`を`Value::BigInt(42)`に変換する処理)は行ごとに変わらないはずです。行に依存しないこの部分を、構築時に1度だけ評価しておく最適化(**定数畳み込み**、第26章で本格的に扱う話題の先取り)を、`FilterExec`のような手書きの演算子1つに限定して試してみてください。
2. `PhysicalPlan::optimize`は現在、`LogicalPlan`を消費して`PhysicalPlan`を返す1対1変換だけを行っています。これに「`Filter`の`predicate`が`FALSE`の定数式なら、その`Filter`とその子を`Values(0 rows)`に置き換える」というルールを追加してみてください(`WHERE FALSE`のような、常に0行になることが計画の時点で分かる`SELECT`を、実行時の`next()`ループを1回も回さずに済ませられるかどうかを確認します)。
